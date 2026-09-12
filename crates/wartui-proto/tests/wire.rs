//! Byte-exact checks on the frames wartui puts on the air.
//!
//! These vectors are written out by hand, and there is nothing else they could be:
//! the wire is ours in both directions, so there is no second implementation to check
//! against — only the rule that a byte written here is a byte a node in the field is
//! already sending, and changing one is changing the protocol. So every vector spells
//! out what it pins.

use wartui_proto::air::{
    ADMIN_FLAG_BLE, ADMIN_MSG_LEN, AdminMsg, Capabilities, DecodeError, Frame, HEARTBEAT_MSG_LEN,
    HeartbeatMsg, MAGIC, RecordKind, SIGHTING_MSG_MAX, SIGHTING_MSG_MIN, SSID_MAX, Security,
    SightingMsg, WIRE_VERSION, foreign,
};
use wartui_proto::plan::{ChannelSet, IndexRun};

/// `WTUI`, version 1, then the type byte.
const HEARTBEAT: &[u8] = &[
    0x57, 0x54, 0x55, 0x49, 0x01, 0x01, // header
    0x78, 0x56, 0x34, 0x12, // counter 0x1234_5678, little-endian
    0x01, 0x00, 0x03, // capabilities: major 1, minor 0, ble + 5g
];

const SIGHTING_WIFI: &[u8] = &[
    0x57, 0x54, 0x55, 0x49, 0x01, 0x02, // header
    0x00, // kind: Wi-Fi
    0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, // bssid
    0x0B, // channel 11
    0xD6, // rssi -42
    0x03, // security: [WPA2_PSK]
    0x06, // ssid_len
    b'M', b'y', b',', b'N', b'e', b't', // and the comma survives
];

const SIGHTING_BLE: &[u8] = &[
    0x57, 0x54, 0x55, 0x49, 0x01, 0x02, // header
    0x01, // kind: BLE
    0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, // address
    0x00, // channel 0, which is what BLE reports
    0xBA, // rssi -70
    0x0A, // security: [BLE]
    0x00, // no SSID
];

const ADMIN: &[u8] = &[
    0x57, 0x54, 0x55, 0x49, 0x01, 0x81, // header
    0x07, // epoch
    0x02, 0x05, // node 2 of 5
    0x00, // flags
    0x00, 0x00, 0xFF, 0x00, 0x00, // indices 16..=23
];

const ADMIN_BLE: &[u8] = &[
    0x57, 0x54, 0x55, 0x49, 0x01, 0x81, // header
    0xC8, // epoch 200
    0x00, 0x01, // node 0 of 1
    0x01, // ADMIN_FLAG_BLE
    0x41, 0x20, 0x00, 0x00, 0x80, // indices 0, 6, 13 and 39
];

/// A stock node's heartbeat, and a stock core's assignment. Kept only as inputs
/// to [`foreign::classify`] — nothing here decodes either.
const VENDOR_HEARTBEAT: &[u8] = &[0x45, 0x4E, 0x4F, 0x57, 0x03, 0x93, 0x00, 0x00, 0x00];
const VENDOR_ADMIN: &[u8] = &[0x45, 0x4E, 0x4F, 0x57, 0x05, 0x07, 0x02, 0x05, 0x10, 0x17];

#[test]
fn the_frames_are_the_lengths_the_constants_promise() {
    assert_eq!(HEARTBEAT.len(), HEARTBEAT_MSG_LEN);
    assert_eq!(ADMIN.len(), ADMIN_MSG_LEN);
    assert_eq!(SIGHTING_BLE.len(), SIGHTING_MSG_MIN, "a sighting with no SSID is the floor");
    assert_eq!(SIGHTING_MSG_MAX, SIGHTING_MSG_MIN + SSID_MAX);
    // The point of the whole exercise: a heartbeat was 212 bytes and a
    // sighting was 212 bytes, on a control channel every node shares.
    const { assert!(HEARTBEAT_MSG_LEN < 20 && SIGHTING_MSG_MAX < 60) };
}

#[test]
fn every_frame_carries_the_same_header() {
    for frame in [HEARTBEAT, SIGHTING_WIFI, SIGHTING_BLE, ADMIN, ADMIN_BLE] {
        assert_eq!(&frame[..4], MAGIC, "magic");
        assert_eq!(frame[4], WIRE_VERSION, "version");
    }
}

#[test]
fn nothing_we_transmit_carries_the_vendors_magic() {
    // A vendor core must not admit one of ours to its node table, nor a vendor node
    // read an assignment out of one.
    for frame in [HEARTBEAT, SIGHTING_WIFI, SIGHTING_BLE, ADMIN, ADMIN_BLE] {
        assert_ne!(&frame[..4], &foreign::VENDOR_MAGIC[..]);
        assert_eq!(foreign::classify(frame), None);
    }
}

#[test]
fn a_heartbeat_encodes_byte_for_byte() {
    let msg = HeartbeatMsg {
        counter: 0x1234_5678,
        capabilities: Capabilities { major: 1, minor: 0, ble: true, five_ghz: true },
    };
    assert_eq!(msg.encode().as_slice(), HEARTBEAT);
    assert_eq!(HeartbeatMsg::decode(HEARTBEAT), Ok(msg));
}

#[test]
fn the_heartbeat_counter_goes_out_least_significant_byte_first() {
    // A counter read big-endian would still be monotonic, so the reboot
    // detector would never notice; only the bytes say which it is.
    let decoded = HeartbeatMsg::decode(HEARTBEAT).expect("valid");
    assert_eq!(decoded.counter, 0x1234_5678);
    assert_eq!(&HEARTBEAT[6..10], &[0x78, 0x56, 0x34, 0x12]);
}

#[test]
fn a_wifi_sighting_encodes_byte_for_byte() {
    let msg = SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        channel: 11,
        rssi: -42,
        security: Security::Wpa2Psk,
        ssid: b"My,Net",
    };
    let mut buf = [0u8; SIGHTING_MSG_MAX];
    let len = msg.encode_into(&mut buf).expect("SIGHTING_MSG_MAX is always enough");
    assert_eq!(&buf[..len], SIGHTING_WIFI);
    assert_eq!(SightingMsg::decode(SIGHTING_WIFI), Ok(msg));
}

#[test]
fn a_comma_in_an_ssid_survives_the_wire() {
    // It did not before: the vendor line was split on commas, so the sender
    // rewrote one as an underscore and the real name was lost at the only
    // point in the path where it still existed.
    let decoded = SightingMsg::decode(SIGHTING_WIFI).expect("valid");
    assert_eq!(decoded.ssid, b"My,Net");
}

#[test]
fn a_ble_sighting_encodes_byte_for_byte() {
    let msg = SightingMsg {
        kind: RecordKind::Ble,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        channel: 0,
        rssi: -70,
        security: Security::Ble,
        ssid: b"",
    };
    let mut buf = [0u8; SIGHTING_MSG_MAX];
    let len = msg.encode_into(&mut buf).expect("fits");
    assert_eq!(&buf[..len], SIGHTING_BLE);
    assert_eq!(SightingMsg::decode(SIGHTING_BLE), Ok(msg));
}

#[test]
fn an_rssi_is_signed_and_a_channel_is_not() {
    // 0xD6 as an unsigned byte is 214, which is a plausible-looking number and
    // a nonsensical dBm. Channel 165 is the other way round: the top of the
    // 5 GHz pool, and a channel read as a signed byte would be -91.
    let decoded = SightingMsg::decode(SIGHTING_WIFI).expect("valid");
    assert_eq!(decoded.rssi, -42);

    let msg = SightingMsg { channel: 165, ..decoded };
    let mut buf = [0u8; SIGHTING_MSG_MAX];
    let len = msg.encode_into(&mut buf).expect("fits");
    assert_eq!(SightingMsg::decode(&buf[..len]).expect("valid").channel, 165);
}

#[test]
fn the_longest_ssid_there_is_fits_the_buffer_sized_for_it() {
    let ssid = [b'x'; SSID_MAX];
    let msg = SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [1, 2, 3, 4, 5, 6],
        channel: 1,
        rssi: -1,
        security: Security::Open,
        ssid: &ssid,
    };
    let mut buf = [0u8; SIGHTING_MSG_MAX];
    let len = msg.encode_into(&mut buf).expect("that is what the constant is for");
    assert_eq!(len, SIGHTING_MSG_MAX);
    assert_eq!(SightingMsg::decode(&buf[..len]).expect("valid").ssid, &ssid[..]);

    // And a buffer one byte short refuses rather than truncating, so a node
    // cannot broadcast a half-formed frame.
    let mut cramped = [0u8; SIGHTING_MSG_MAX - 1];
    assert_eq!(msg.encode_into(&mut cramped), None);
}

#[test]
fn an_ssid_longer_than_the_standard_allows_is_refused_at_both_ends() {
    let ssid = [b'x'; SSID_MAX + 1];
    let msg = SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [1, 2, 3, 4, 5, 6],
        channel: 1,
        rssi: -1,
        security: Security::Open,
        ssid: &ssid,
    };
    let mut buf = [0u8; 128];
    assert_eq!(msg.encode_into(&mut buf), None);

    let mut frame = SIGHTING_BLE.to_vec();
    frame[16] = 33;
    assert_eq!(SightingMsg::decode(&frame), Err(DecodeError::SsidTooLong(33)));
}

#[test]
fn a_truncated_ssid_is_short_rather_than_silently_empty() {
    // `ssid_len` says six and five arrived. Reading what is there would file
    // an access point under a name it never had.
    let mut frame = SIGHTING_WIFI.to_vec();
    frame.pop();
    assert_eq!(
        SightingMsg::decode(&frame),
        Err(DecodeError::TooShort { need: SIGHTING_MSG_MIN + 6, got: SIGHTING_MSG_MIN + 5 })
    );
}

#[test]
fn a_security_value_this_build_never_heard_of_does_not_cost_the_observation() {
    // A node from a later build reporting a mode this host has no name for
    // still reported an access point, and the address is the part that matters.
    let mut frame = SIGHTING_BLE.to_vec();
    frame[15] = 200;
    let decoded = SightingMsg::decode(&frame).expect("still a sighting");
    assert_eq!(decoded.security, Security::Unknown(200));
    assert_eq!(decoded.security.token(), None);
    assert_eq!(decoded.security.to_string(), "[UNKNOWN:200]");
}

#[test]
fn every_security_value_round_trips_and_spells_itself_the_wigle_way() {
    // These tokens leave the host in the `AuthMode` column, so they are still a
    // format even though they no longer travel on the wire.
    let named = [
        (Security::Open, "[OPEN]"),
        (Security::Wep, "[WEP]"),
        (Security::WpaPsk, "[WPA_PSK]"),
        (Security::Wpa2Psk, "[WPA2_PSK]"),
        (Security::WpaWpa2Psk, "[WPA_WPA2_PSK]"),
        (Security::Wpa2Enterprise, "[WPA2]"),
        (Security::Wpa3Psk, "[WPA3_PSK]"),
        (Security::Wpa2Wpa3Psk, "[WPA2_WPA3_PSK]"),
        (Security::WapiPsk, "[WAPI_PSK]"),
        (Security::Undefined, "[UNDEFINED]"),
        (Security::Ble, "[BLE]"),
    ];
    for (security, token) in named {
        assert_eq!(security.token(), Some(token));
        assert_eq!(Security::from_u8(security.as_u8()), security);
    }
    // And `Unknown` never shadows one of the named values, so the two
    // conversions are inverses over the whole byte range.
    for raw in 0..=u8::MAX {
        assert_eq!(Security::from_u8(raw).as_u8(), raw);
    }
}

#[test]
fn an_assignment_encodes_byte_for_byte() {
    let msg = AdminMsg {
        epoch: 7,
        node_index: 2,
        node_count: 5,
        flags: 0,
        channels: ChannelSet::from_run(IndexRun::new(16, 23)),
    };
    assert_eq!(msg.encode().as_slice(), ADMIN);
    assert_eq!(AdminMsg::decode(ADMIN), Ok(msg));
}

#[test]
fn the_channel_mask_goes_out_least_significant_byte_first() {
    // Four indices chosen to straddle byte boundaries and to reach the top of
    // the forty, so a mask written big-endian — or in four bytes rather than
    // five — cannot produce these bytes.
    let mut channels = ChannelSet::empty();
    for idx in [0, 6, 13, 39] {
        channels.insert(idx);
    }
    let msg =
        AdminMsg { epoch: 200, node_index: 0, node_count: 1, flags: ADMIN_FLAG_BLE, channels };
    assert_eq!(msg.encode().as_slice(), ADMIN_BLE);

    let back = AdminMsg::decode(ADMIN_BLE).expect("valid");
    assert_eq!(back.channels.indices().collect::<Vec<_>>(), vec![0, 6, 13, 39]);
    assert!(back.scan_ble());
}

#[test]
fn an_unknown_flag_bit_survives_a_decode_and_re_encode() {
    // A newer host setting a bit this build has no name for must not have it
    // quietly dropped on the way through.
    let mut frame = ADMIN_BLE.to_vec();
    frame[9] = 0b1000_0001;
    let decoded = AdminMsg::decode(&frame).expect("valid");
    assert_eq!(decoded.flags, 0b1000_0001);
    assert!(decoded.scan_ble());
    assert_eq!(decoded.encode().as_slice(), frame.as_slice());
}

#[test]
fn frame_decode_routes_every_type_to_its_own_layout() {
    assert!(matches!(Frame::decode(HEARTBEAT), Ok(Frame::Heartbeat(_))));
    assert!(matches!(Frame::decode(SIGHTING_WIFI), Ok(Frame::Sighting(_))));
    assert!(matches!(Frame::decode(SIGHTING_BLE), Ok(Frame::Sighting(_))));
    assert!(matches!(Frame::decode(ADMIN), Ok(Frame::Admin(_))));
}

#[test]
fn a_decoder_refuses_a_frame_of_the_wrong_type() {
    // Each decoder is reachable directly, and reading an assignment out of a
    // heartbeat would be adopting four bytes of counter as a channel mask.
    assert_eq!(AdminMsg::decode(HEARTBEAT), Err(DecodeError::UnknownType(0x01)));
    assert_eq!(HeartbeatMsg::decode(ADMIN), Err(DecodeError::UnknownType(0x81)));
    assert_eq!(SightingMsg::decode(ADMIN), Err(DecodeError::UnknownType(0x81)));
}

#[test]
fn bad_magic_is_rejected_before_anything_else_is_read() {
    let mut frame = HEARTBEAT.to_vec();
    frame[0] = b'X';
    assert_eq!(Frame::decode(&frame), Err(DecodeError::BadMagic));
}

#[test]
fn a_version_this_build_does_not_know_is_named_rather_than_guessed_at() {
    // The vendor header had no version field, which is exactly why its
    // ten-byte assignment could pass a longer frame's length check and decode
    // as something plausible. This is the field that stops that happening here.
    let mut frame = HEARTBEAT.to_vec();
    frame[4] = 2;
    assert_eq!(Frame::decode(&frame), Err(DecodeError::BadVersion(2)));

    let mut frame = ADMIN.to_vec();
    frame[4] = 0;
    assert_eq!(AdminMsg::decode(&frame), Err(DecodeError::BadVersion(0)));
}

#[test]
fn an_unknown_type_byte_is_rejected() {
    let mut frame = HEARTBEAT.to_vec();
    frame[5] = 0x42;
    assert_eq!(Frame::decode(&frame), Err(DecodeError::UnknownType(0x42)));
}

#[test]
fn short_frames_are_rejected_by_the_layout_they_claim_to_be() {
    assert_eq!(Frame::decode(&HEARTBEAT[..4]), Err(DecodeError::TooShort { need: 6, got: 4 }));
    assert_eq!(
        Frame::decode(&HEARTBEAT[..HEARTBEAT_MSG_LEN - 1]),
        Err(DecodeError::TooShort { need: HEARTBEAT_MSG_LEN, got: HEARTBEAT_MSG_LEN - 1 })
    );
    assert_eq!(
        Frame::decode(&ADMIN[..ADMIN_MSG_LEN - 1]),
        Err(DecodeError::TooShort { need: ADMIN_MSG_LEN, got: ADMIN_MSG_LEN - 1 })
    );
    assert_eq!(
        Frame::decode(&SIGHTING_BLE[..SIGHTING_MSG_MIN - 1]),
        Err(DecodeError::TooShort { need: SIGHTING_MSG_MIN, got: SIGHTING_MSG_MIN - 1 })
    );
}

#[test]
fn the_vendors_traffic_is_recognised_without_being_decoded() {
    // Another fleet on the control channel is an operational fact — it is
    // transmitting where these nodes are listening — so counting it as line
    // noise would hide the one clue an operator has.
    assert_eq!(foreign::classify(VENDOR_ADMIN), Some(foreign::Foreign::Admin));
    assert_eq!(foreign::classify(VENDOR_HEARTBEAT), Some(foreign::Foreign::Node));

    // And it is not decodable here, which is the point.
    assert_eq!(Frame::decode(VENDOR_ADMIN), Err(DecodeError::BadMagic));
    assert_eq!(Frame::decode(VENDOR_HEARTBEAT), Err(DecodeError::BadMagic));
}

#[test]
fn nothing_shorter_than_a_vendor_header_is_mistaken_for_one() {
    assert_eq!(foreign::classify(&VENDOR_ADMIN[..4]), None, "magic alone names no type");
    assert_eq!(foreign::classify(b""), None);
    assert_eq!(foreign::classify(b"not espnow at all"), None);
}
