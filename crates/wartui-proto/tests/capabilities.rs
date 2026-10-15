//! What a node says it is, in every heartbeat it sends.
//!
//! A version, a board and two feature bits, since the magic is what separates one of
//! ours from anything else. The rule that matters: a node from a later build announcing
//! something this host has never heard of is still a node.

use wartui_proto::air::{
    Board, CAP_FLAG_5G, CAP_FLAG_TRIAL, Capabilities, DieTemp, HeartbeatMsg, NODE_VERSION, Vdd,
    Version,
};
use wartui_proto::reset::ResetCause;

fn round_trip(caps: Capabilities) -> Capabilities {
    let frame = HeartbeatMsg {
        counter: 0,
        epoch: 0,
        capabilities: caps,
        wifi_refused: 0,
        ble_refused: 0,
        beat: 1,
        unsent: 0,
        dwell: 0,
        prev_dwell: 0,
        tx_power: 8,
        reset_cause: ResetCause::PowerOn,
        vdd: Vdd::NONE,
        die_temp: DieTemp::NONE,
    }
    .encode();
    HeartbeatMsg::decode(&frame).expect("we just encoded it").capabilities
}

#[test]
fn capabilities_survives_heartbeat_round_trip_when_encoded_and_decoded() {
    for board in [Board::Esp32c5, Board::Esp32c6, Board::Esp32c6Xiao, Board::Unknown(0x7F)] {
        let caps = Capabilities::here(board);
        assert_eq!(round_trip(caps), caps, "{caps}");
    }
    let trial = Capabilities { trial: true, ..Capabilities::here(Board::Esp32c5) };
    assert_eq!(round_trip(trial), trial);
}

#[test]
fn capabilities_maps_feature_bits_when_converting_to_flags() {
    // Written out rather than derived, because these are wire values: the node
    // sets them and the host reads them, and this pins what both ends agree on.
    assert_eq!(CAP_FLAG_5G, 0b01);
    assert_eq!(CAP_FLAG_TRIAL, 0b10);
    assert_eq!(Capabilities::here(Board::Esp32c6).flags(), 0);
    assert_eq!(Capabilities::here(Board::Esp32c6Xiao).flags(), 0);
    assert_eq!(Capabilities::here(Board::Esp32c5).flags(), CAP_FLAG_5G);
    let trial = Capabilities { trial: true, ..Capabilities::here(Board::Esp32c5) };
    assert_eq!(trial.flags(), CAP_FLAG_5G | CAP_FLAG_TRIAL);
}

#[test]
fn capabilities_preserves_known_flags_when_parsing_unknown_feature_bits() {
    // Dropping such a node would take its share of the pool out of the fleet.
    let caps = Capabilities::from_prefix([0, 9, 2, 1, 0b1111_1111]);
    assert_eq!(caps.version, Version::new(0, 9, 2));
    assert!(caps.five_ghz && caps.trial, "and the bits it does know still read");

    let bare = Capabilities::from_prefix([0, 1, 0, 2, 0b1000_0000]);
    assert!(!bare.five_ghz && !bare.trial);
}

#[test]
fn board_maps_bytes_when_converting_to_and_from_wire() {
    // Wire values, frozen with the prefix that carries them.
    assert_eq!(Board::Esp32c5.as_u8(), 1);
    assert_eq!(Board::Esp32c6.as_u8(), 2);
    assert_eq!(Board::Esp32c6Xiao.as_u8(), 3);
    for byte in 0..=u8::MAX {
        assert_eq!(Board::from_u8(byte).as_u8(), byte);
    }
}

#[test]
fn board_decodes_unknown_when_byte_unassigned() {
    // A board added later is still a node, not a decode error.
    assert_eq!(Board::from_u8(0), Board::Unknown(0));
    assert_eq!(Board::from_u8(0x42), Board::Unknown(0x42));
}

#[test]
fn capabilities_carries_node_version_when_constructed() {
    let caps = Capabilities::here(Board::Esp32c5);
    assert_eq!(caps.version, NODE_VERSION);
    assert!(!caps.trial, "no firmware sets the trial bit yet");
}

#[test]
fn capabilities_reads_five_ghz_from_board_when_constructed() {
    assert!(Capabilities::here(Board::Esp32c5).five_ghz);
    assert!(!Capabilities::here(Board::Esp32c6).five_ghz);
    assert!(!Capabilities::here(Board::Esp32c6Xiao).five_ghz);
}

#[test]
fn capabilities_formats_as_display_token_when_rendered_as_string() {
    // The fleet table, the logs and the store column all read this.
    let c5 = Capabilities::here(Board::Esp32c5);
    let c6 = Capabilities::here(Board::Esp32c6);
    let version = Version::new(0, 1, 0);
    assert_eq!(Capabilities { version, ..c5 }.to_string(), "0.1.0;5g");
    assert_eq!(Capabilities { version, ..c6 }.to_string(), "0.1.0");
    assert_eq!(Capabilities { version, trial: true, ..c5 }.to_string(), "0.1.0;5g;trial");
    assert_eq!(
        Capabilities { version: Version::new(255, 255, 255), ..c6 }.to_string(),
        "255.255.255"
    );
}
