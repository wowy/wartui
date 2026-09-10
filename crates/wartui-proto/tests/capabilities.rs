//! What a node says it is, in every heartbeat it sends.
//!
//! Three bytes now rather than an ASCII token in a field the vendor left empty.
//! The magic separates one of ours from anything else, so what is left here is
//! a version and two feature bits — and the rule that mattered most survives
//! the change: a node from a later build announcing something this host has
//! never heard of is still a node, and must not be dropped out of the fleet
//! for it.

use wartui_proto::air::{
    CAP_FLAG_5G, CAP_FLAG_BLE, CAPABILITY_MAJOR, CAPABILITY_MINOR, Capabilities, HeartbeatMsg,
};

fn round_trip(caps: Capabilities) -> Capabilities {
    let frame = HeartbeatMsg { counter: 0, capabilities: caps }.encode();
    HeartbeatMsg::decode(&frame).expect("we just encoded it").capabilities
}

#[test]
fn capabilities_round_trip_through_a_heartbeat() {
    for (ble, five) in [(false, false), (true, false), (false, true), (true, true)] {
        let caps = Capabilities::here(ble, five);
        assert_eq!(round_trip(caps), caps, "{caps}");
    }
}

#[test]
fn the_feature_bits_are_the_ones_the_plan_named() {
    // Written out rather than derived, because these are wire values: a node in
    // the field sets them and a host that changed its mind would misread it.
    assert_eq!(Capabilities::here(false, false).flags(), 0);
    assert_eq!(Capabilities::here(true, false).flags(), CAP_FLAG_BLE);
    assert_eq!(Capabilities::here(false, true).flags(), CAP_FLAG_5G);
    assert_eq!(Capabilities::here(true, true).flags(), 0b11);
}

#[test]
fn a_feature_this_host_has_never_heard_of_does_not_disqualify_a_node() {
    // The whole reason unknown bits are ignored. A node from a later build
    // announcing something new is still a node, and dropping it would take its
    // share of the pool out of the fleet.
    let caps = Capabilities::from_parts(1, 9, 0b1111_1111);
    assert_eq!((caps.major, caps.minor), (1, 9));
    assert!(caps.ble && caps.five_ghz, "and the ones it does know still read");

    let bare = Capabilities::from_parts(2, 0, 0b1000_0000);
    assert!(!bare.ble && !bare.five_ghz);
}

#[test]
fn this_build_announces_the_version_it_speaks() {
    let caps = Capabilities::here(true, true);
    assert_eq!((caps.major, caps.minor), (CAPABILITY_MAJOR, CAPABILITY_MINOR));
}

#[test]
fn capabilities_are_shown_the_way_the_old_token_was_spelled() {
    // The fleet table and the store column both read this. It is no longer a
    // wire format, but an operator who has seen one fleet table should be able
    // to read the next one.
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
