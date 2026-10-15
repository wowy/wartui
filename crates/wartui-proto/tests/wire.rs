//! Byte-exact checks on the frames wartui puts on the air.
//!
//! These vectors are written out by hand, and there is nothing else they could be:
//! the wire is ours in both directions, so there is no second implementation to check
//! against — only the rule that a byte written here is a byte a node in the field is
//! already sending, and changing one is changing the protocol. So every vector spells
//! out what it pins.

use wartui_proto::air::{
    ADMIN_FLAG_BLE, ADMIN_MSG_LEN, AdminMsg, Board, CLEAR_MSG_LEN, Capabilities, ClearMsg,
    DecodeError, DieTemp, EXT_MAX, Frame, HEARTBEAT_MSG_LEN, HEARTBEAT_PREFIX_LEN, HeartbeatMsg,
    MAGIC, RecordKind, SIGHTING_BATCH_HEADER, SIGHTING_BATCH_MAX, SIGHTING_RECORD_MAX,
    SIGHTING_RECORD_MIN, SIGHTINGS_PER_BATCH_MAX, SSID_MAX, Security, SightingBatch,
    SightingBatchWriter, SightingMsg, Vdd, Version, WIRE_VERSION, foreign,
};
use wartui_proto::plan::{ChannelSet, IndexRun};
use wartui_proto::reset::ResetCause;

/// `WTUI`, version 1, then the type byte.
const HEARTBEAT: &[u8] = &[
    0x57, 0x54, 0x55, 0x49, 0x01, 0x01, // header
    0x00, 0x01, 0x02, // version 0.1.2
    0x01, // board: ESP32-C5
    0x01, // flags: 5g
    0x78, 0x56, 0x34, 0x12, // counter 0x1234_5678, little-endian
    0x05, // epoch
    0x2A, 0x00, // wifi_refused 42, little-endian
    0x04, 0x03, // ble_refused 0x0304, little-endian
    0x02, 0x01, // beat 0x0102, little-endian
    0x07, // unsent 7
    0x95, // dwell: channel 149
    0x24, // prev_dwell: channel 36
    0x18, // tx_power: 24 quarter-dBm, 6 dBm
    0x05, // reset_cause: brownout
    0x43, // vdd: code 67, 3304 mV
    0x51, // die_temp: code 81, 41 °C
];

/// A heartbeat from a node parked since boot: epoch 0, never sent by the host.
const HEARTBEAT_NO_EPOCH: &[u8] = &[
    0x57, 0x54, 0x55, 0x49, 0x01, 0x01, // header
    0x00, 0x01, 0x00, // version 0.1.0
    0x03, // board: XIAO ESP32-C6, external antenna
    0x02, // flags: trial, 2.4 GHz only
    0x78, 0x56, 0x34, 0x12, // counter 0x1234_5678, little-endian
    0x00, // epoch: none held
    0x00, 0x00, // wifi_refused 0
    0x00, 0x00, // ble_refused 0
    0x01, 0x00, // beat 1: the first heartbeat since boot
    0x00, // unsent 0
    0x00, // dwell: none, the node is parked
    0x00, // prev_dwell: none, the first heartbeat since boot
    0x08, // tx_power: 8 quarter-dBm, the 2 dBm boot default
    0x01, // reset_cause: power on
    0x00, // vdd: no reading
    0xFF, // die_temp: no reading
];

/// A Wi-Fi record: no header of its own, since [`SightingBatch`] carries one
/// for the whole frame.
const WIFI_RECORD: &[u8] = &[
    0x00, // kind: Wi-Fi
    0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, // bssid
    0x0B, // channel 11
    0xD6, // rssi -42
    0x03, // security: [WPA2_PSK]
    0x06, // ssid_len
    b'M', b'y', b',', b'N', b'e', b't', // and the comma survives
    0x00, // no trailer: this access point beaconed no roaming consortium
];

const BLE_RECORD: &[u8] = &[
    0x01, // kind: BLE
    0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, // address
    0x00, // channel 0, which is what BLE reports
    0xBA, // rssi -70
    0x0A, // security: [BLE]
    0x00, // no SSID
    0x00, // no trailer: this advertiser carried no manufacturer data
];

/// A Wi-Fi record whose beacon carried the OpenRoaming roaming consortium
/// triple, verbatim in the trailer: count, the nibble-packed lengths, then
/// three five-byte identifiers. The widest record there is.
const WIFI_RCOI_RECORD: &[u8] = &[
    0x00, // kind: Wi-Fi
    0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, // bssid
    0x0B, // channel 11
    0xD6, // rssi -42
    0x03, // security: [WPA2_PSK]
    0x06, // ssid_len
    b'M', b'y', b',', b'N', b'e', b't', 0x11, // ext_len: 17, the most there is
    0x02, // ANQP OI count
    0x55, // OI lengths: five and five, the third implicit
    0x5A, 0x03, 0xBA, 0x00, 0x00, // 5A03BA0000
    0xBA, 0xA2, 0xD0, 0x00, 0x00, // BAA2D00000
    0xBA, 0xA2, 0xD0, 0x20, 0x00, // BAA2D02000
];

/// A BLE record from an advertiser that carried a manufacturer identifier.
const BLE_MFGR_RECORD: &[u8] = &[
    0x01, // kind: BLE
    0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, // address
    0x00, // channel 0
    0xBA, // rssi -70
    0x0A, // security: [BLE]
    0x00, // no SSID
    0x02, // ext_len: the identifier, and nothing else
    0x4C, 0x00, // company identifier 76, little-endian
];

/// One batch, carrying the one Wi-Fi record above.
const SIGHTING_BATCH_1: &[u8] = &[
    0x57, 0x54, 0x55, 0x49, 0x01, 0x02, // header
    0x01, 0x00, // seq 1, little-endian
    0x01, // count
    0x00, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x0B, 0xD6, 0x03, 0x06, b'M', b'y', b',', b'N', b'e',
    b't', 0x00, // WIFI_RECORD
];

/// A single-record BLE batch, for the tests that want `Frame::Sightings` on a
/// BLE record without a Wi-Fi one alongside it.
const SIGHTING_BATCH_BLE: &[u8] = &[
    0x57, 0x54, 0x55, 0x49, 0x01, 0x02, // header
    0x09, 0x00, // seq 9
    0x01, // count
    0x01, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0xBA, 0x0A, 0x00, 0x00, // BLE_RECORD
];

/// A mixed batch: Wi-Fi, then BLE, then the widest record there is.
const SIGHTING_BATCH_3: &[u8] = &[
    0x57, 0x54, 0x55, 0x49, 0x01, 0x02, // header
    0x07, 0x00, // seq 7
    0x03, // count
    // WIFI_RECORD
    0x00, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x0B, 0xD6, 0x03, 0x06, b'M', b'y', b',', b'N', b'e',
    b't', 0x00, //
    // BLE_RECORD
    0x01, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0xBA, 0x0A, 0x00, 0x00, //
    // WIFI_RCOI_RECORD
    0x00, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x0B, 0xD6, 0x03, 0x06, b'M', b'y', b',', b'N', b'e',
    b't', 0x11, 0x02, 0x55, 0x5A, 0x03, 0xBA, 0x00, 0x00, 0xBA, 0xA2, 0xD0, 0x00, 0x00, 0xBA, 0xA2,
    0xD0, 0x20, 0x00,
];

const ADMIN: &[u8] = &[
    0x57, 0x54, 0x55, 0x49, 0x01, 0x81, // header
    0x07, // epoch
    0x00, // flags
    0x00, 0x00, 0xFF, 0x00, 0x00, 0x00, // indices 16..=23
    0x08, // transmit power: 2 dBm
];

const ADMIN_BLE: &[u8] = &[
    0x57, 0x54, 0x55, 0x49, 0x01, 0x81, // header
    0xC8, // epoch 200
    0x01, // ADMIN_FLAG_BLE
    0x41, 0x20, 0x00, 0x00, 0x00, 0x02, // indices 0, 6, 13 and 41
    0x08, // transmit power: 2 dBm
];

/// The assignment a node whose whole job is Bluetooth is sent: the flag, and an
/// empty [`ChannelSet`]. The one frame carrying a mask of zero, and the reason an
/// empty mask means "Bluetooth is all of it" rather than "scan nothing".
const ADMIN_BLE_ONLY: &[u8] = &[
    0x57, 0x54, 0x55, 0x49, 0x01, 0x81, // header
    0x09, // epoch
    0x01, // ADMIN_FLAG_BLE
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // no channels at all
    0x08, // transmit power: 2 dBm
];

/// Header only: the whole instruction is the type byte.
const CLEAR: &[u8] = &[0x57, 0x54, 0x55, 0x49, 0x01, 0x82];

/// A stock node's heartbeat, and a stock core's assignment. Kept only as inputs
/// to [`foreign::classify`] — nothing here decodes either.
const VENDOR_HEARTBEAT: &[u8] = &[0x45, 0x4E, 0x4F, 0x57, 0x03, 0x93, 0x00, 0x00, 0x00];
const VENDOR_ADMIN: &[u8] = &[0x45, 0x4E, 0x4F, 0x57, 0x05, 0x07, 0x02, 0x05, 0x10, 0x17];

#[test]
fn wire_codec_matches_expected_frame_lengths_when_checking_constants() {
    assert_eq!(HEARTBEAT.len(), HEARTBEAT_MSG_LEN);
    assert_eq!(ADMIN.len(), ADMIN_MSG_LEN);
    assert_eq!(
        BLE_RECORD.len(),
        SIGHTING_RECORD_MIN,
        "a record with no SSID and no trailer is the floor"
    );
    assert_eq!(
        WIFI_RCOI_RECORD.len(),
        SIGHTING_RECORD_MIN + 6 + EXT_MAX,
        "the widest trailer there is"
    );
    assert_eq!(SIGHTING_RECORD_MAX, SIGHTING_RECORD_MIN + SSID_MAX + EXT_MAX);
    assert_eq!(
        SIGHTING_BATCH_1.len(),
        SIGHTING_BATCH_HEADER + WIFI_RECORD.len(),
        "the header plus the one record it carries"
    );
    assert_eq!(CLEAR.len(), CLEAR_MSG_LEN, "header only, nothing else");
    // The point of the whole exercise: a heartbeat was 212 bytes and a
    // sighting was 212 bytes, on a control channel every node shares. The
    // ceiling is the 250 bytes every node frame keeps to, and a batch is packed
    // to fill exactly that many.
    const {
        assert!(HEARTBEAT_MSG_LEN < 32 && SIGHTING_RECORD_MAX < SIGHTING_BATCH_MAX);
        assert!(SIGHTING_BATCH_MAX == 250);
    };
}

#[test]
fn wire_codec_includes_magic_and_version_header_when_inspecting_frames() {
    for frame in
        [HEARTBEAT, SIGHTING_BATCH_1, SIGHTING_BATCH_BLE, SIGHTING_BATCH_3, ADMIN, ADMIN_BLE, CLEAR]
    {
        assert_eq!(&frame[..4], MAGIC, "magic");
        assert_eq!(frame[4], WIRE_VERSION, "version");
    }
}

#[test]
fn foreign_classifier_rejects_our_frames_when_checking_vendor_magic() {
    // A vendor core must not admit one of ours to its node table, nor a vendor node
    // read an assignment out of one.
    for frame in
        [HEARTBEAT, SIGHTING_BATCH_1, SIGHTING_BATCH_BLE, SIGHTING_BATCH_3, ADMIN, ADMIN_BLE, CLEAR]
    {
        assert_ne!(&frame[..4], &foreign::VENDOR_MAGIC[..]);
        assert_eq!(foreign::classify(frame), None);
    }
}

#[test]
fn heartbeat_msg_serializes_byte_for_byte_when_encoded_and_decoded() {
    let msg = HeartbeatMsg {
        counter: 0x1234_5678,
        epoch: 5,
        capabilities: Capabilities {
            version: Version::new(0, 1, 2),
            board: Board::Esp32c5,
            five_ghz: true,
            trial: false,
        },
        wifi_refused: 42,
        ble_refused: 0x0304,
        beat: 0x0102,
        unsent: 7,
        dwell: 149,
        prev_dwell: 36,
        tx_power: 24,
        reset_cause: ResetCause::Brownout,
        vdd: Vdd(67),
        die_temp: DieTemp(81),
    };
    assert_eq!(msg.encode().as_slice(), HEARTBEAT);
    assert_eq!(HeartbeatMsg::decode(HEARTBEAT), Ok(msg));
}

#[test]
fn heartbeat_msg_serializes_epoch_zero_when_node_holds_no_assignment() {
    let msg = HeartbeatMsg {
        counter: 0x1234_5678,
        epoch: 0,
        capabilities: Capabilities {
            version: Version::new(0, 1, 0),
            board: Board::Esp32c6Xiao,
            five_ghz: false,
            trial: true,
        },
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
    };
    assert_eq!(msg.encode().as_slice(), HEARTBEAT_NO_EPOCH);
    assert_eq!(HeartbeatMsg::decode(HEARTBEAT_NO_EPOCH), Ok(msg));
}

#[test]
fn heartbeat_msg_serializes_counter_as_little_endian_when_encoded() {
    // A counter read big-endian would still be monotonic, so the reboot
    // detector would never notice; only the bytes say which it is.
    let decoded = HeartbeatMsg::decode(HEARTBEAT).expect("valid");
    assert_eq!(decoded.counter, 0x1234_5678);
    assert_eq!(&HEARTBEAT[11..15], &[0x78, 0x56, 0x34, 0x12]);
}

#[test]
fn heartbeat_msg_serializes_refusal_counts_as_little_endian_when_encoded() {
    let decoded = HeartbeatMsg::decode(HEARTBEAT).expect("valid");
    assert_eq!(decoded.wifi_refused, 42);
    assert_eq!(decoded.ble_refused, 0x0304);
    assert_eq!(&HEARTBEAT[16..18], &[0x2A, 0]);
    assert_eq!(&HEARTBEAT[18..20], &[0x04, 0x03]);
}

#[test]
fn heartbeat_msg_round_trips_beat_when_encoded_at_its_offset() {
    let decoded = HeartbeatMsg::decode(HEARTBEAT).expect("valid");
    assert_eq!(decoded.beat, 0x0102);
    assert_eq!(&HEARTBEAT[20..22], &[0x02, 0x01]);

    let msg = HeartbeatMsg { beat: u16::MAX, ..decoded };
    let frame = msg.encode();
    assert_eq!(&frame[20..22], &[0xFF, 0xFF]);
    assert_eq!(HeartbeatMsg::decode(&frame), Ok(msg));
}

#[test]
fn heartbeat_msg_round_trips_unsent_when_encoded_at_its_offset() {
    let decoded = HeartbeatMsg::decode(HEARTBEAT).expect("valid");
    assert_eq!(decoded.unsent, 7);
    assert_eq!(HEARTBEAT[22], 0x07);

    let msg = HeartbeatMsg { unsent: u8::MAX, ..decoded };
    let frame = msg.encode();
    assert_eq!(frame[22], 0xFF);
    assert_eq!(HeartbeatMsg::decode(&frame), Ok(msg));
}

#[test]
fn heartbeat_msg_round_trips_dwell_when_encoded_at_its_offset() {
    let decoded = HeartbeatMsg::decode(HEARTBEAT).expect("valid");
    assert_eq!((decoded.dwell, decoded.prev_dwell), (149, 36));
    assert_eq!(&HEARTBEAT[23..25], &[0x95, 0x24]);

    let msg = HeartbeatMsg { dwell: 6, prev_dwell: 165, ..decoded };
    let frame = msg.encode();
    assert_eq!(&frame[23..25], &[0x06, 0xA5]);
    assert_eq!(HeartbeatMsg::decode(&frame), Ok(msg));
}

#[test]
fn heartbeat_msg_round_trips_tx_power_when_encoded_at_its_offset() {
    let decoded = HeartbeatMsg::decode(HEARTBEAT).expect("valid");
    assert_eq!(decoded.tx_power, 24);
    assert_eq!(HEARTBEAT[25], 0x18);

    // 0 is what a node sends when it could not read its radio's power.
    for tx_power in [8, 80, 0] {
        let msg = HeartbeatMsg { tx_power, ..decoded };
        let frame = msg.encode();
        assert_eq!(frame[25], tx_power.to_le_bytes()[0]);
        assert_eq!(HeartbeatMsg::decode(&frame), Ok(msg));
    }
}

#[test]
fn heartbeat_msg_round_trips_reset_cause_when_encoded_at_its_offset() {
    let decoded = HeartbeatMsg::decode(HEARTBEAT).expect("valid");
    assert_eq!(decoded.reset_cause, ResetCause::Brownout);
    assert_eq!(HEARTBEAT[26], 0x05);

    for reset_cause in [
        ResetCause::PowerOn,
        ResetCause::Software,
        ResetCause::Watchdog,
        ResetCause::Lockup,
        ResetCause::Brownout,
        ResetCause::External,
        ResetCause::Unknown,
    ] {
        let msg = HeartbeatMsg { reset_cause, ..decoded };
        let frame = msg.encode();
        assert_eq!(frame[26], reset_cause.as_u8());
        assert_eq!(HeartbeatMsg::decode(&frame), Ok(msg));
    }
}

#[test]
fn heartbeat_msg_round_trips_health_when_encoded_at_its_offsets() {
    let decoded = HeartbeatMsg::decode(HEARTBEAT).expect("valid");
    assert_eq!((decoded.vdd.mv(), decoded.die_temp.c()), (Some(3304), Some(41)));
    assert_eq!(&HEARTBEAT[27..29], &[0x43, 0x51]);

    let msg = HeartbeatMsg { vdd: Vdd::NONE, die_temp: DieTemp::NONE, ..decoded };
    let frame = msg.encode();
    assert_eq!(&frame[27..29], &[0x00, 0xFF]);
    assert_eq!(HeartbeatMsg::decode(&frame), Ok(msg));
}

#[test]
fn vdd_encodes_no_reading_when_read_returns_zero() {
    assert_eq!(Vdd::from_mv(0), Vdd::NONE);
    assert_eq!(Vdd::NONE.mv(), None);
}

#[test]
fn vdd_rounds_to_nearest_step_when_encoding_millivolts() {
    // 800 mV above the base is 66.7 steps, so 67: 3304 mV.
    assert_eq!(Vdd::from_mv(3300), Vdd(67));
    assert_eq!(Vdd::from_mv(3300).mv(), Some(3304));
}

#[test]
fn vdd_clamps_to_encodable_range_when_reading_is_outside_it() {
    // A real reading never becomes the no-reading code.
    assert_eq!(Vdd::from_mv(2500), Vdd(1));
    assert_eq!(Vdd::from_mv(1), Vdd(1));
    assert_eq!(Vdd::from_mv(5560), Vdd(255));
    assert_eq!(Vdd::from_mv(9000), Vdd(255));
    assert_eq!(Vdd(255).mv(), Some(5560));
}

#[test]
fn vdd_displays_volts_when_formatted() {
    assert_eq!(Vdd::from_mv(3300).to_string(), "3.30 V");
    assert_eq!(Vdd::NONE.to_string(), "—");
}

#[test]
fn die_temp_encodes_whole_degrees_when_reading_is_in_range() {
    assert_eq!(DieTemp::from_c(-40), DieTemp(0));
    assert_eq!(DieTemp::from_c(41), DieTemp(81));
    assert_eq!(DieTemp::from_c(214), DieTemp(254));
    assert_eq!(DieTemp(81).c(), Some(41));
    assert_eq!(DieTemp(0).c(), Some(-40));
}

#[test]
fn die_temp_clamps_to_encodable_range_when_reading_is_outside_it() {
    assert_eq!(DieTemp::from_c(-41), DieTemp(0));
    // Above 214 °C is 254, never the no-reading code.
    assert_eq!(DieTemp::from_c(300), DieTemp(254));
}

#[test]
fn die_temp_decodes_no_reading_when_code_is_0xff() {
    assert_eq!(DieTemp(0xFF).c(), None);
    assert_eq!(DieTemp::NONE.to_string(), "—");
}

#[test]
fn die_temp_displays_celsius_when_formatted() {
    assert_eq!(DieTemp::from_c(41).to_string(), "41 °C");
    assert_eq!(DieTemp::from_c(-5).to_string(), "-5 °C");
}

#[test]
fn heartbeat_msg_decodes_unknown_reset_cause_when_byte_unassigned() {
    // A node naming a cause this build has no name for is still a node.
    let mut frame = HEARTBEAT.to_vec();
    frame[26] = 0xEE;
    let decoded = HeartbeatMsg::decode(&frame).expect("valid");
    assert_eq!(decoded.reset_cause, ResetCause::Unknown);
}

#[test]
fn heartbeat_msg_rejects_frame_when_node_sends_twenty_five_byte_heartbeat() {
    // A node flashed from another tree, whose heartbeat is shorter. The engine counts
    // this as incompatible and asks for a reflash.
    assert_eq!(
        HeartbeatMsg::decode(&HEARTBEAT[..25]),
        Err(DecodeError::BadLength { need: HEARTBEAT_MSG_LEN, got: 25 })
    );
    assert_eq!(
        Frame::decode(&HEARTBEAT[..25]),
        Err(DecodeError::BadLength { need: HEARTBEAT_MSG_LEN, got: 25 })
    );
}

#[test]
fn heartbeat_msg_rejects_frame_when_node_sends_thirty_byte_heartbeat() {
    // A longer frame is the dangerous half, because its leading bytes would parse.
    let mut longer = HEARTBEAT.to_vec();
    longer.push(0);
    assert_eq!(
        HeartbeatMsg::decode(&longer),
        Err(DecodeError::BadLength { need: HEARTBEAT_MSG_LEN, got: 30 })
    );
    assert_eq!(
        Frame::decode(&longer),
        Err(DecodeError::BadLength { need: HEARTBEAT_MSG_LEN, got: 30 })
    );
}

#[test]
fn sighting_msg_serializes_wifi_record_byte_for_byte_when_encoded_and_decoded() {
    let msg = SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        channel: 11,
        rssi: -42,
        security: Security::Wpa2Psk,
        ssid: b"My,Net",
        ext: &[],
    };
    let mut buf = [0u8; SIGHTING_RECORD_MAX];
    let len = msg.encode_record_into(&mut buf).expect("SIGHTING_RECORD_MAX is always enough");
    assert_eq!(&buf[..len], WIFI_RECORD);
    assert_eq!(SightingMsg::decode_record(WIFI_RECORD), Ok((msg, WIFI_RECORD.len())));
}

#[test]
fn sighting_msg_serializes_roaming_consortium_trailer_when_encoded() {
    // The trailer is the element's body verbatim — count byte and lengths
    // byte included — so a change to how it is read costs a re-export, never
    // a re-flash.
    const OPEN_ROAMING: &[u8] = &[
        0x02, 0x55, //
        0x5A, 0x03, 0xBA, 0x00, 0x00, //
        0xBA, 0xA2, 0xD0, 0x00, 0x00, //
        0xBA, 0xA2, 0xD0, 0x20, 0x00,
    ];
    let msg = SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        channel: 11,
        rssi: -42,
        security: Security::Wpa2Psk,
        ssid: b"My,Net",
        ext: OPEN_ROAMING,
    };
    let mut buf = [0u8; SIGHTING_RECORD_MAX];
    let len = msg.encode_record_into(&mut buf).expect("fits");
    assert_eq!(&buf[..len], WIFI_RCOI_RECORD);
    let (back, consumed) = SightingMsg::decode_record(WIFI_RCOI_RECORD).expect("valid");
    assert_eq!(consumed, WIFI_RCOI_RECORD.len());
    assert_eq!(back, msg);
    assert_eq!(back.ext, OPEN_ROAMING, "the body survives the wire unchanged");
}

#[test]
fn sighting_msg_preserves_comma_in_ssid_when_decoded_from_wire() {
    // The SSID is length-prefixed, so nothing has to be escaped or rewritten.
    let (decoded, _) = SightingMsg::decode_record(WIFI_RECORD).expect("valid");
    assert_eq!(decoded.ssid, b"My,Net");
}

#[test]
fn sighting_msg_serializes_ble_record_byte_for_byte_when_encoded_and_decoded() {
    let msg = SightingMsg {
        kind: RecordKind::Ble,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        channel: 0,
        rssi: -70,
        security: Security::Ble,
        ssid: b"",
        ext: &[],
    };
    let mut buf = [0u8; SIGHTING_RECORD_MAX];
    let len = msg.encode_record_into(&mut buf).expect("fits");
    assert_eq!(&buf[..len], BLE_RECORD);
    assert_eq!(SightingMsg::decode_record(BLE_RECORD), Ok((msg, BLE_RECORD.len())));
}

#[test]
fn sighting_msg_serializes_ble_manufacturer_trailer_when_encoded() {
    // Two little-endian bytes and nothing else: the trailer for a BLE
    // sighting is the identifier or it is empty, never anything in between.
    let msg = SightingMsg {
        kind: RecordKind::Ble,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        channel: 0,
        rssi: -70,
        security: Security::Ble,
        ssid: b"",
        ext: &76u16.to_le_bytes(),
    };
    let mut buf = [0u8; SIGHTING_RECORD_MAX];
    let len = msg.encode_record_into(&mut buf).expect("fits");
    assert_eq!(&buf[..len], BLE_MFGR_RECORD);
    assert_eq!(SightingMsg::decode_record(BLE_MFGR_RECORD), Ok((msg, BLE_MFGR_RECORD.len())));
}

#[test]
fn sighting_msg_parses_signed_rssi_and_unsigned_channel_when_decoded() {
    // 0xD6 as an unsigned byte is 214, which is a plausible-looking number and
    // a nonsensical dBm. Channel 165 is the other way round: the top of the
    // 5 GHz pool, and a channel read as a signed byte would be -91.
    let (decoded, _) = SightingMsg::decode_record(WIFI_RECORD).expect("valid");
    assert_eq!(decoded.rssi, -42);

    let msg = SightingMsg { channel: 165, ..decoded };
    let mut buf = [0u8; SIGHTING_RECORD_MAX];
    let len = msg.encode_record_into(&mut buf).expect("fits");
    assert_eq!(SightingMsg::decode_record(&buf[..len]).expect("valid").0.channel, 165);
}

#[test]
fn sighting_msg_fits_max_buffer_when_ssid_and_trailer_are_longest() {
    let ssid = [b'x'; SSID_MAX];
    let ext = [0xEE; EXT_MAX];
    let msg = SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [1, 2, 3, 4, 5, 6],
        channel: 1,
        rssi: -1,
        security: Security::Open,
        ssid: &ssid,
        ext: &ext,
    };
    let mut buf = [0u8; SIGHTING_RECORD_MAX];
    let len = msg.encode_record_into(&mut buf).expect("that is what the constant is for");
    assert_eq!(len, SIGHTING_RECORD_MAX);
    let (back, consumed) = SightingMsg::decode_record(&buf[..len]).expect("valid");
    assert_eq!(consumed, len);
    assert_eq!(back.ssid, &ssid[..]);
    assert_eq!(back.ext, &ext[..]);

    // And a buffer one byte short refuses rather than truncating, so a node
    // cannot broadcast a half-formed record.
    let mut cramped = [0u8; SIGHTING_RECORD_MAX - 1];
    assert_eq!(msg.encode_record_into(&mut cramped), None);
}

#[test]
fn sighting_msg_rejects_oversized_ssid_when_encoding_or_decoding() {
    let ssid = [b'x'; SSID_MAX + 1];
    let msg = SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [1, 2, 3, 4, 5, 6],
        channel: 1,
        rssi: -1,
        security: Security::Open,
        ssid: &ssid,
        ext: &[],
    };
    let mut buf = [0u8; 128];
    assert_eq!(msg.encode_record_into(&mut buf), None);

    let mut record = BLE_RECORD.to_vec();
    record[10] = 33; // ssid_len
    assert_eq!(SightingMsg::decode_record(&record), Err(DecodeError::SsidTooLong(33)));
}

#[test]
fn sighting_msg_rejects_oversized_trailer_when_encoding_or_decoding() {
    // Refused on the way in, so a node cannot broadcast it, and on the way
    // out, so a frame from a build with a wider trailer is named rather than
    // half-read.
    let msg = SightingMsg {
        kind: RecordKind::Ble,
        bssid: [1, 2, 3, 4, 5, 6],
        channel: 0,
        rssi: -1,
        security: Security::Ble,
        ssid: b"",
        ext: &[0xEE; EXT_MAX + 1],
    };
    let mut buf = [0u8; 128];
    assert_eq!(msg.encode_record_into(&mut buf), None);

    let mut record = WIFI_RECORD.to_vec();
    record[17] = 18; // ext_len
    assert_eq!(SightingMsg::decode_record(&record), Err(DecodeError::ExtTooLong(18)));
}

#[test]
fn sighting_msg_returns_too_short_error_when_trailer_length_is_truncated() {
    // `ext_len` says two and one arrived. Reading what is there would file an
    // identifier nobody sent.
    let mut record = BLE_MFGR_RECORD.to_vec();
    record.pop();
    assert_eq!(
        SightingMsg::decode_record(&record),
        Err(DecodeError::TooShort { need: SIGHTING_RECORD_MIN + 2, got: SIGHTING_RECORD_MIN + 1 })
    );
}

#[test]
fn sighting_msg_returns_too_short_error_when_ssid_is_truncated() {
    // `ssid_len` says six and five arrived. Reading what is there would file
    // an access point under a name it never had.
    let mut record = WIFI_RECORD.to_vec();
    record.pop();
    assert_eq!(
        SightingMsg::decode_record(&record),
        Err(DecodeError::TooShort { need: SIGHTING_RECORD_MIN + 6, got: SIGHTING_RECORD_MIN + 5 })
    );
}

#[test]
fn sighting_msg_preserves_sighting_with_unknown_security_when_decoded() {
    // A node from a later build reporting a mode this host has no name for
    // still reported an access point, and the address is the part that matters.
    let mut record = BLE_RECORD.to_vec();
    record[9] = 200; // security
    let (decoded, _) = SightingMsg::decode_record(&record).expect("still a sighting");
    assert_eq!(decoded.security, Security::Unknown(200));
    assert_eq!(decoded.security.token(), None);
    assert_eq!(decoded.security.to_string(), "[UNKNOWN:200]");
}

#[test]
fn sighting_batch_writer_packs_one_record_byte_for_byte_when_built() {
    let mut writer = SightingBatchWriter::new(1);
    assert!(writer.push(&SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        channel: 11,
        rssi: -42,
        security: Security::Wpa2Psk,
        ssid: b"My,Net",
        ext: &[],
    }));
    assert_eq!(writer.len(), 1);
    assert_eq!(writer.as_bytes(), SIGHTING_BATCH_1);

    let batch = SightingBatch::decode(SIGHTING_BATCH_1).expect("valid");
    assert_eq!(batch.seq, 1);
    assert_eq!(batch.count, 1);
    let records: Vec<_> = batch.iter().collect();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].1, WIFI_RECORD);
}

#[test]
fn sighting_batch_writer_packs_mixed_records_byte_for_byte_when_built() {
    let mut writer = SightingBatchWriter::new(7);
    assert!(writer.push(&SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        channel: 11,
        rssi: -42,
        security: Security::Wpa2Psk,
        ssid: b"My,Net",
        ext: &[],
    }));
    assert!(writer.push(&SightingMsg {
        kind: RecordKind::Ble,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        channel: 0,
        rssi: -70,
        security: Security::Ble,
        ssid: b"",
        ext: &[],
    }));
    // The trailer bytes of `WIFI_RCOI_RECORD`, past its own header-free fields.
    assert!(writer.push(&SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        channel: 11,
        rssi: -42,
        security: Security::Wpa2Psk,
        ssid: b"My,Net",
        ext: &WIFI_RCOI_RECORD[18..],
    }));
    assert_eq!(writer.as_bytes(), SIGHTING_BATCH_3);

    let batch = SightingBatch::decode(SIGHTING_BATCH_3).expect("valid");
    let raw: Vec<&[u8]> = batch.iter().map(|(_, raw)| raw).collect();
    assert_eq!(raw, std::vec![WIFI_RECORD, BLE_RECORD, WIFI_RCOI_RECORD]);
}

#[test]
fn sighting_batch_writer_fills_to_batch_ceiling_when_packing_minimal_records() {
    // Nineteen bare records and one carrying a one-byte SSID sum to exactly
    // 241 bytes behind the nine-byte header: 250 total, the batch ceiling,
    // reached at exactly SIGHTINGS_PER_BATCH_MAX records.
    let minimal = SightingMsg {
        kind: RecordKind::Ble,
        bssid: [0; 6],
        channel: 0,
        rssi: -1,
        security: Security::Ble,
        ssid: b"",
        ext: &[],
    };
    let mut writer = SightingBatchWriter::new(1);
    for _ in 0..SIGHTINGS_PER_BATCH_MAX - 1 {
        assert!(writer.push(&minimal));
    }
    assert!(writer.push(&SightingMsg { ssid: b"x", ..minimal }));
    assert_eq!(writer.len(), SIGHTINGS_PER_BATCH_MAX);
    assert_eq!(writer.as_bytes().len(), SIGHTING_BATCH_MAX);

    // One more record, however small, no longer fits: nothing is written, and
    // the frame already on the air is unchanged.
    assert!(!writer.push(&minimal));
    assert_eq!(writer.len(), SIGHTINGS_PER_BATCH_MAX, "the refused push wrote nothing");
    assert_eq!(writer.as_bytes().len(), SIGHTING_BATCH_MAX);

    let batch = SightingBatch::decode(writer.as_bytes()).expect("valid");
    assert_eq!(batch.iter().count(), SIGHTINGS_PER_BATCH_MAX);
}

#[test]
fn sighting_batch_rejects_frame_when_count_is_zero() {
    let mut frame = SIGHTING_BATCH_1[..SIGHTING_BATCH_HEADER].to_vec();
    frame[8] = 0; // count
    assert_eq!(
        SightingBatch::decode(&frame),
        Err(DecodeError::TooShort {
            need: SIGHTING_BATCH_HEADER + SIGHTING_RECORD_MIN,
            got: frame.len(),
        })
    );
}

#[test]
fn sighting_batch_rejects_frame_when_count_exceeds_records_present() {
    let mut frame = SIGHTING_BATCH_1.to_vec();
    frame[8] = 2; // claims a second record that is not there
    assert_eq!(
        SightingBatch::decode(&frame),
        Err(DecodeError::TooShort { need: SIGHTING_RECORD_MIN, got: 0 })
    );
}

#[test]
fn sighting_batch_rejects_frame_when_trailing_bytes_follow_last_record() {
    let mut frame = SIGHTING_BATCH_1.to_vec();
    frame.extend_from_slice(BLE_RECORD); // count still says 1
    assert_eq!(
        SightingBatch::decode(&frame),
        Err(DecodeError::BadLength {
            need: SIGHTING_BATCH_HEADER + WIFI_RECORD.len(),
            got: frame.len(),
        })
    );
}

#[test]
fn sighting_batch_rejects_whole_frame_when_second_record_is_malformed() {
    // Validated whole before anything is yielded: a fault in the second
    // record must not let the first through.
    let second = SIGHTING_BATCH_HEADER + WIFI_RECORD.len();

    let mut bad_ssid = SIGHTING_BATCH_3.to_vec();
    bad_ssid[second + 10] = 33; // the second record's ssid_len
    assert_eq!(SightingBatch::decode(&bad_ssid), Err(DecodeError::SsidTooLong(33)));

    let mut bad_kind = SIGHTING_BATCH_3.to_vec();
    bad_kind[second] = 2; // no RecordKind is 2
    assert_eq!(SightingBatch::decode(&bad_kind), Err(DecodeError::UnknownType(2)));
}

#[test]
fn security_enum_round_trips_to_wigle_tokens_when_converted() {
    // These tokens leave the host in the `AuthMode` column, so they are a format
    // even though they never travel on the wire.
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
fn assignment_serializes_byte_for_byte_when_encoded_to_wire() {
    let msg = AdminMsg {
        epoch: 7,
        flags: 0,
        channels: ChannelSet::from_run(IndexRun::new(16, 23)),
        tx_power: 8,
    };
    assert_eq!(msg.encode().as_slice(), ADMIN);
    assert_eq!(AdminMsg::decode(ADMIN), Ok(msg));
}

#[test]
fn admin_msg_serializes_channel_mask_as_little_endian_when_encoded() {
    // Four indices chosen to straddle byte boundaries and to reach the top of
    // the forty-two, so a mask written big-endian — or in five bytes rather
    // than six — cannot produce these bytes.
    let mut channels = ChannelSet::empty();
    for idx in [0, 6, 13, 41] {
        channels.insert(idx);
    }
    let msg = AdminMsg { epoch: 200, flags: ADMIN_FLAG_BLE, channels, tx_power: 8 };
    assert_eq!(msg.encode().as_slice(), ADMIN_BLE);

    let back = AdminMsg::decode(ADMIN_BLE).expect("valid");
    assert_eq!(back.channels.indices().collect::<Vec<_>>(), vec![0, 6, 13, 41]);
    assert!(back.scan_ble());
}

#[test]
fn admin_msg_encodes_ble_flag_and_empty_mask_when_node_scans_bluetooth() {
    let msg =
        AdminMsg { epoch: 9, flags: ADMIN_FLAG_BLE, channels: ChannelSet::empty(), tx_power: 8 };
    assert_eq!(msg.encode().as_slice(), ADMIN_BLE_ONLY);

    let back = AdminMsg::decode(ADMIN_BLE_ONLY).expect("valid");
    assert!(back.channels.is_empty(), "it sniffs nothing");
    assert!(back.scan_ble(), "and the flag is what says why");
    assert_eq!(back.tx_power, 8);
}

#[test]
fn admin_msg_preserves_unknown_flag_bits_when_round_tripped() {
    // A newer host setting a bit this build has no name for must not have it
    // quietly dropped on the way through.
    let mut frame = ADMIN_BLE.to_vec();
    frame[7] = 0b1000_0001;
    let decoded = AdminMsg::decode(&frame).expect("valid");
    assert_eq!(decoded.flags, 0b1000_0001);
    assert!(decoded.scan_ble());
    assert_eq!(decoded.encode().as_slice(), frame.as_slice());
}

#[test]
fn clear_msg_serializes_byte_for_byte_when_encoded_and_decoded() {
    assert_eq!(ClearMsg.encode().as_slice(), CLEAR);
    assert_eq!(ClearMsg::decode(CLEAR), Ok(ClearMsg));
}

#[test]
fn clear_msg_rejects_frame_when_length_is_not_header_only() {
    // Longer is the dangerous half, the same reasoning `BadLength` exists for
    // everywhere else: a wider frame from a later build must not be adopted as
    // a plausible clear.
    let mut wider = CLEAR.to_vec();
    wider.push(0xFF);
    assert_eq!(
        ClearMsg::decode(&wider),
        Err(DecodeError::BadLength { need: CLEAR_MSG_LEN, got: CLEAR_MSG_LEN + 1 })
    );

    let shorter = &CLEAR[..CLEAR_MSG_LEN - 1];
    assert_eq!(
        ClearMsg::decode(shorter),
        Err(DecodeError::TooShort { need: CLEAR_MSG_LEN, got: CLEAR_MSG_LEN - 1 })
    );
}

#[test]
fn frame_decoder_dispatches_type_byte_to_corresponding_frame_when_decoded() {
    assert!(matches!(Frame::decode(HEARTBEAT), Ok(Frame::Heartbeat(_))));
    assert!(matches!(Frame::decode(SIGHTING_BATCH_1), Ok(Frame::Sightings(_))));
    assert!(matches!(Frame::decode(SIGHTING_BATCH_BLE), Ok(Frame::Sightings(_))));
    assert!(matches!(Frame::decode(ADMIN), Ok(Frame::Admin(_))));
    assert!(matches!(Frame::decode(CLEAR), Ok(Frame::Clear(_))));
}

#[test]
fn frame_decoder_rejects_payload_when_frame_type_mismatches() {
    // Each decoder is reachable directly, and reading an assignment out of a
    // heartbeat would be adopting four bytes of counter as a channel mask.
    assert_eq!(AdminMsg::decode(HEARTBEAT), Err(DecodeError::UnknownType(0x01)));
    assert_eq!(HeartbeatMsg::decode(ADMIN), Err(DecodeError::UnknownType(0x81)));
    assert_eq!(SightingBatch::decode(ADMIN), Err(DecodeError::UnknownType(0x81)));
}

#[test]
fn wire_decoder_rejects_frame_when_magic_bytes_do_not_match() {
    let mut frame = HEARTBEAT.to_vec();
    frame[0] = b'X';
    assert_eq!(Frame::decode(&frame), Err(DecodeError::BadMagic));
}

#[test]
fn wire_decoder_rejects_frame_when_wire_version_is_unsupported() {
    // Without a version field, a frame of another shape could pass a length
    // check and decode as something plausible. This is the field that stops that happening here.
    let mut frame = HEARTBEAT.to_vec();
    frame[4] = 2;
    assert_eq!(Frame::decode(&frame), Err(DecodeError::BadVersion(2)));

    let mut frame = ADMIN.to_vec();
    frame[4] = 0;
    assert_eq!(AdminMsg::decode(&frame), Err(DecodeError::BadVersion(0)));
}

#[test]
fn wire_decoder_rejects_frame_when_type_byte_is_unknown() {
    let mut frame = HEARTBEAT.to_vec();
    frame[5] = 0x42;
    assert_eq!(Frame::decode(&frame), Err(DecodeError::UnknownType(0x42)));
}

#[test]
fn wire_decoder_rejects_truncated_payload_when_frame_is_too_short() {
    // Too short to carry a header at all, which is as far as `header` gets.
    assert_eq!(Frame::decode(&HEARTBEAT[..4]), Err(DecodeError::TooShort { need: 6, got: 4 }));
    // A batch needs its own header past the shared one: seq and count.
    assert_eq!(
        Frame::decode(&SIGHTING_BATCH_1[..SIGHTING_BATCH_HEADER - 1]),
        Err(DecodeError::TooShort { need: SIGHTING_BATCH_HEADER, got: SIGHTING_BATCH_HEADER - 1 })
    );
    // The other two are each one size, so either side of it is a layout this
    // build does not read.
    assert_eq!(
        Frame::decode(&HEARTBEAT[..HEARTBEAT_MSG_LEN - 1]),
        Err(DecodeError::BadLength { need: HEARTBEAT_MSG_LEN, got: HEARTBEAT_MSG_LEN - 1 })
    );
    assert_eq!(
        Frame::decode(&ADMIN[..ADMIN_MSG_LEN - 1]),
        Err(DecodeError::BadLength { need: ADMIN_MSG_LEN, got: ADMIN_MSG_LEN - 1 })
    );
}

#[test]
fn wire_codec_pins_heartbeat_prefix_when_inspecting_frames() {
    // The frozen core: header, heartbeat type byte, then version, board and flags. A host
    // a minor version ahead reads these bytes from this build's nodes, so none may move.
    assert_eq!(HEARTBEAT_MSG_LEN, 29);
    assert_eq!(HEARTBEAT_PREFIX_LEN, 5);
    assert_eq!(&HEARTBEAT[..11], &[0x57, 0x54, 0x55, 0x49, 0x01, 0x01, 0, 1, 2, 1, 1]);
}

#[test]
fn heartbeat_prefix_decodes_version_board_and_flags_when_frame_is_exact() {
    let prefix = Capabilities::decode_prefix(HEARTBEAT).expect("a heartbeat");
    assert_eq!(prefix, HeartbeatMsg::decode(HEARTBEAT).expect("valid").capabilities);
    assert_eq!(prefix.version, Version::new(0, 1, 2));
    assert_eq!(prefix.board, Board::Esp32c5);
}

#[test]
fn heartbeat_prefix_decodes_when_body_is_any_length_past_prefix() {
    // An older or newer minor's heartbeat: the prefix is all that is read.
    let older = [0x57, 0x54, 0x55, 0x49, 0x01, 0x01, 0, 0, 7, 2, 0];
    let caps = Capabilities::decode_prefix(&older).expect("prefix only");
    assert_eq!(caps.version, Version::new(0, 0, 7));
    assert_eq!(caps.board, Board::Esp32c6);
    assert!(!caps.five_ghz);

    let mut longer = HEARTBEAT.to_vec();
    longer.extend_from_slice(&[0xAA; 40]);
    assert_eq!(Capabilities::decode_prefix(&longer), Capabilities::decode_prefix(HEARTBEAT));
}

#[test]
fn heartbeat_prefix_rejects_frame_when_body_is_shorter_than_prefix() {
    assert_eq!(
        Capabilities::decode_prefix(&HEARTBEAT[..10]),
        Err(DecodeError::TooShort { need: 11, got: 10 })
    );
}

#[test]
fn heartbeat_prefix_rejects_frame_when_board_is_zero() {
    // No build sends board 0; a heartbeat without the prefix puts its sweep counter's high
    // byte there.
    let mut frame = HEARTBEAT.to_vec();
    frame[9] = 0;
    assert_eq!(Capabilities::decode_prefix(&frame), Err(DecodeError::NoBoard));
    assert_eq!(HeartbeatMsg::decode(&frame), Err(DecodeError::NoBoard));
    assert_eq!(Capabilities::decode_prefix(&frame[..11]), Err(DecodeError::NoBoard));
}

#[test]
fn heartbeat_prefix_accepts_unknown_flag_bits_when_board_is_named() {
    // A later minor may define a flag; its nodes must still be found.
    let mut frame = HEARTBEAT.to_vec();
    frame[10] = 0b1000_0001;
    assert!(Capabilities::decode_prefix(&frame).is_ok_and(|caps| caps.five_ghz));
}

#[test]
fn heartbeat_prefix_rejects_frame_when_not_a_heartbeat() {
    assert_eq!(Capabilities::decode_prefix(ADMIN), Err(DecodeError::UnknownType(0x81)));
    let mut bad_version = HEARTBEAT.to_vec();
    bad_version[4] = 2;
    assert_eq!(Capabilities::decode_prefix(&bad_version), Err(DecodeError::BadVersion(2)));
    let mut bad_magic = HEARTBEAT.to_vec();
    bad_magic[0] = b'X';
    assert_eq!(Capabilities::decode_prefix(&bad_magic), Err(DecodeError::BadMagic));
}

#[test]
fn wire_decoder_rejects_payload_when_fixed_frame_exceeds_expected_length() {
    // The half of a layout change that would otherwise be silent: a wider
    // assignment from a newer host has a valid header, a known type byte and
    // enough bytes for every field this build knows, so a length check is the
    // only thing between it and being adopted as a plausible wrong share.
    // `WIRE_VERSION` does not move before 1.0, so this is that check.
    let mut wider = ADMIN.to_vec();
    wider.push(0xFF);
    assert_eq!(
        Frame::decode(&wider),
        Err(DecodeError::BadLength { need: ADMIN_MSG_LEN, got: ADMIN_MSG_LEN + 1 })
    );

    let mut wider = HEARTBEAT.to_vec();
    wider.push(0xFF);
    assert_eq!(
        Frame::decode(&wider),
        Err(DecodeError::BadLength { need: HEARTBEAT_MSG_LEN, got: HEARTBEAT_MSG_LEN + 1 })
    );
}

#[test]
fn foreign_classifier_identifies_vendor_traffic_when_foreign_frames_arrive() {
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
fn foreign_classifier_returns_none_when_input_is_shorter_than_vendor_header() {
    assert_eq!(foreign::classify(&VENDOR_ADMIN[..4]), None, "magic alone names no type");
    assert_eq!(foreign::classify(b""), None);
    assert_eq!(foreign::classify(b"not espnow at all"), None);
}
