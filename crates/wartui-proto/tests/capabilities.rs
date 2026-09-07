//! The token a node puts in every heartbeat to say what it is.
//!
//! It is the only thing separating one of ours from a stock node, because node
//! to core is byte-identical by design. So the parser has to be strict about
//! what counts as one of ours and forgiving about everything else: a node from
//! a later build that has grown a feature this host never heard of is still a
//! node, and must not be dropped out of the fleet for it.

use wartui_proto::air::{CAPABILITY_MAJOR, CAPABILITY_MAX, CAPABILITY_MINOR, Capabilities};

fn token(caps: &Capabilities) -> String {
    let mut buf = [0u8; CAPABILITY_MAX];
    let len = caps.write_into(&mut buf).expect("CAPABILITY_MAX is always enough");
    String::from_utf8(buf[..len].to_vec()).expect("the token is ASCII")
}

#[test]
fn a_token_round_trips_through_the_text_field() {
    for (ble, five) in [(false, false), (true, false), (false, true), (true, true)] {
        let caps = Capabilities::here(ble, five);
        let text = token(&caps);
        assert_eq!(Capabilities::parse(text.as_bytes()), Some(caps), "{text}");
    }
}

#[test]
fn the_token_reads_the_way_the_plan_wrote_it() {
    // Written out rather than derived, because this string is a wire format:
    // a node in the field speaks it and a host that changed its mind about the
    // spelling would stop recognising that node.
    assert_eq!(
        token(&Capabilities { major: 0, minor: 1, ble: true, five_ghz: true }),
        "wartui/0.1;ble,5g"
    );
    assert_eq!(
        token(&Capabilities { major: 0, minor: 1, ble: true, five_ghz: false }),
        "wartui/0.1;ble"
    );
    assert_eq!(
        token(&Capabilities { major: 0, minor: 1, ble: false, five_ghz: true }),
        "wartui/0.1;5g"
    );
    assert_eq!(
        token(&Capabilities { major: 0, minor: 1, ble: false, five_ghz: false }),
        "wartui/0.1"
    );
}

#[test]
fn a_heartbeat_from_anything_else_is_simply_not_one_of_ours() {
    // A stock node leaves the text field empty. None of these is an error
    // worth reporting — the host just cannot use the node.
    for text in [
        &b""[..],
        b"wartui",
        b"wartui/",
        b"wartui/0",
        b"wartui/x.1",
        b"wartui/0.",
        b"wartui/.1",
        b"wartui/0.1.2",
        b"wartui/999.1",
        b"WARTUI/0.1",
        b" wartui/0.1",
        b"33:44:55:66:77:88,net,[WPA2_PSK],6,-40,W",
    ] {
        assert_eq!(Capabilities::parse(text), None, "{:?}", core::str::from_utf8(text));
    }
}

#[test]
fn a_feature_this_host_has_never_heard_of_does_not_disqualify_a_node() {
    // The whole reason the list is a list. A node from a later build announcing
    // something new is still a node, and dropping it would take its share of
    // the pool out of the fleet.
    let caps = Capabilities::parse(b"wartui/0.9;ble,lora,5g,gps").expect("still one of ours");
    assert_eq!((caps.major, caps.minor), (0, 9));
    assert!(caps.ble && caps.five_ghz, "and the ones it does know still read");

    let bare = Capabilities::parse(b"wartui/1.0;quantum").expect("still one of ours");
    assert!(!bare.ble && !bare.five_ghz);
}

#[test]
fn a_malformed_feature_list_does_not_cost_the_version() {
    // Version first, features second, so garbage after the semicolon cannot
    // make a well-formed node unreadable.
    let caps = Capabilities::parse(b"wartui/0.1;,,,ble,,").expect("one of ours");
    assert_eq!((caps.major, caps.minor, caps.ble), (0, 1, true));
}

#[test]
fn this_build_announces_the_version_it_speaks() {
    let caps = Capabilities::here(true, true);
    assert_eq!((caps.major, caps.minor), (CAPABILITY_MAJOR, CAPABILITY_MINOR));
}

#[test]
fn the_longest_token_fits_the_buffer_that_is_sized_for_it() {
    let widest = Capabilities { major: 255, minor: 255, ble: true, five_ghz: true };
    let mut buf = [0u8; CAPABILITY_MAX];
    let len = widest.write_into(&mut buf).expect("that is what the constant is for");
    assert_eq!(&buf[..len], b"wartui/255.255;ble,5g");
    assert!(len <= CAPABILITY_MAX);

    let mut cramped = [0u8; 8];
    assert_eq!(widest.write_into(&mut cramped), None, "and it refuses rather than truncating");
}
