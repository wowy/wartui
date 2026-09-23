//! What a node says it is, in every heartbeat it sends.
//!
//! A version and two feature bits, since the magic is what separates one of ours
//! from anything else. The rule that matters: a node from a later build announcing
//! something this host has never heard of is still a node.

use wartui_proto::air::{
    CAP_FLAG_5G, CAP_FLAG_BLE, CAPABILITY_MAJOR, CAPABILITY_MINOR, Capabilities, HeartbeatMsg,
};

fn round_trip(caps: Capabilities) -> Capabilities {
    let frame = HeartbeatMsg { counter: 0, capabilities: caps }.encode();
    HeartbeatMsg::decode(&frame).expect("we just encoded it").capabilities
}

#[test]
fn capabilities_survives_heartbeat_round_trip_when_encoded_and_decoded() {
    for (ble, five) in [(false, false), (true, false), (false, true), (true, true)] {
        let caps = Capabilities::here(ble, five);
        assert_eq!(round_trip(caps), caps, "{caps}");
    }
}

#[test]
fn capabilities_maps_feature_bits_when_converting_to_flags() {
    // Written out rather than derived, because these are wire values: a node in
    // the field sets them and a host that changed its mind would misread it.
    assert_eq!(Capabilities::here(false, false).flags(), 0);
    assert_eq!(Capabilities::here(true, false).flags(), CAP_FLAG_BLE);
    assert_eq!(Capabilities::here(false, true).flags(), CAP_FLAG_5G);
    assert_eq!(Capabilities::here(true, true).flags(), 0b11);
}

#[test]
fn capabilities_preserves_known_flags_when_parsing_unknown_feature_bits() {
    // Dropping such a node would take its share of the pool out of the fleet.
    let caps = Capabilities::from_parts(1, 9, 0b1111_1111);
    assert_eq!((caps.major, caps.minor), (1, 9));
    assert!(caps.ble && caps.five_ghz, "and the ones it does know still read");

    let bare = Capabilities::from_parts(2, 0, 0b1000_0000);
    assert!(!bare.ble && !bare.five_ghz);
}

#[test]
fn capabilities_matches_current_major_and_minor_constants_when_constructed() {
    let caps = Capabilities::here(true, true);
    assert_eq!((caps.major, caps.minor), (CAPABILITY_MAJOR, CAPABILITY_MINOR));
}

#[test]
fn capabilities_formats_as_display_token_when_rendered_as_string() {
    // The fleet table and the store column both read this: not a wire format, but an
    // operator who has read one fleet table should be able to read the next.
    assert_eq!(
        Capabilities { major: 1, minor: 0, ble: true, five_ghz: true }.to_string(),
        "wartui/1.0;ble,5g"
    );
    assert_eq!(
        Capabilities { major: 1, minor: 0, ble: true, five_ghz: false }.to_string(),
        "wartui/1.0;ble"
    );
    assert_eq!(
        Capabilities { major: 1, minor: 0, ble: false, five_ghz: true }.to_string(),
        "wartui/1.0;5g"
    );
    assert_eq!(
        Capabilities { major: 255, minor: 255, ble: false, five_ghz: false }.to_string(),
        "wartui/255.255"
    );
}
