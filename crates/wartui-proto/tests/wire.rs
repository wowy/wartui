//! Byte-exact checks on the message bodies node-bus records carry.
//!
//! These vectors are written out by hand, and there is nothing else they could be:
//! the wire is ours in both directions, so there is no second implementation to check
//! against — only the rule that a byte written here is a byte a node is already sending,
//! and changing one is changing the protocol. So every vector spells out what it pins.

use wartui_proto::bus::{
    ASSIGN_FLAG_BLE, ASSIGN_LEN, Assign, Board, CLEAR_LEN, Capabilities, Clear, DecodeError,
    DieTemp, DownMsg, DownType, EXT_MAX, HEARTBEAT_IDENTITY_LEN, HEARTBEAT_LEN,
    HEARTBEAT_PREFIX_LEN, Heartbeat, RecordKind, SIGHTING_BATCH_HEADER, SIGHTING_BATCH_MAX,
    SIGHTING_RECORD_MAX, SIGHTING_RECORD_MIN, SIGHTINGS_PER_BATCH_MAX, SSID_MAX, Security,
    SightingBatch, SightingBatchWriter, SightingMsg, UPDATE_ABORT_LEN, UPDATE_BEGIN_LEN,
    UPDATE_CHUNK, UPDATE_CHUNK_HEADER, UPDATE_CHUNK_MAX, UPDATE_COMMIT_LEN, UPDATE_CONFIRM_LEN,
    UPDATE_STATUS_LEN, UpMsg, UpType, UpdateAbort, UpdateBegin, UpdateChunk, UpdateCommit,
    UpdateConfirm, UpdateState, UpdateStatus, Vdd, Version,
};
use wartui_proto::link::{MAX_NODE_DOWN, MAX_NODE_PAYLOAD};
use wartui_proto::plan::{ChannelSet, IndexRun};
use wartui_proto::reset::ResetCause;

const HEARTBEAT: &[u8] = &[
    0x00, 0x01, 0x02, // version 0.1.2
    0x01, // board: ESP32-C5
    0x01, // flags: 5g
    0x02, 0x00, 0x00, 0x00, 0x0D, 0x20, // mac 02:00:00:00:0D:20
    0x78, 0x56, 0x34, 0x12, // counter 0x1234_5678, little-endian
    0x05, // epoch
    0x2A, 0x00, // wifi_refused 42, little-endian
    0x04, 0x03, // ble_refused 0x0304, little-endian
    0x05, // reset_cause: brownout
    0x43, // vdd: code 67, 3304 mV
    0x51, // die_temp: code 81, 41 °C
];

/// A heartbeat from a node unassigned since boot: epoch 0, never sent by the host.
const HEARTBEAT_NO_EPOCH: &[u8] = &[
    0x00, 0x01, 0x00, // version 0.1.0
    0x01, // board: ESP32-C5
    0x02, // flags: trial, 5 GHz clear
    0x02, 0x00, 0x00, 0x00, 0x1A, 0x50, // mac 02:00:00:00:1A:50
    0x78, 0x56, 0x34, 0x12, // counter 0x1234_5678, little-endian
    0x00, // epoch: none held
    0x00, 0x00, // wifi_refused 0
    0x00, 0x00, // ble_refused 0
    0x01, // reset_cause: power on
    0x00, // vdd: no reading
    0xFF, // die_temp: no reading
];

/// A Wi-Fi record: no header of its own, since [`SightingBatch`] carries one
/// for the whole batch.
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
    0x01, 0x00, // seq 1, little-endian
    0x01, // count
    0x00, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x0B, 0xD6, 0x03, 0x06, b'M', b'y', b',', b'N', b'e',
    b't', 0x00, // WIFI_RECORD
];

/// A single-record BLE batch, for the tests that want `UpMsg::Sightings` on a
/// BLE record without a Wi-Fi one alongside it.
const SIGHTING_BATCH_BLE: &[u8] = &[
    0x09, 0x00, // seq 9
    0x01, // count
    0x01, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0xBA, 0x0A, 0x00, 0x00, // BLE_RECORD
];

/// A mixed batch: Wi-Fi, then BLE, then the widest record there is.
const SIGHTING_BATCH_3: &[u8] = &[
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

const ASSIGN: &[u8] = &[
    0x07, // epoch
    0x00, // flags
    0x00, 0x00, 0xFF, 0x00, 0x00, 0x00, // indices 16..=23
];

const ASSIGN_BLE: &[u8] = &[
    0xC8, // epoch 200
    0x01, // ASSIGN_FLAG_BLE
    0x41, 0x20, 0x00, 0x00, 0x00, 0x02, // indices 0, 6, 13 and 41
];

/// The assignment a node whose whole job is Bluetooth is sent: the flag, and an
/// empty [`ChannelSet`]. The one assignment carrying a mask of zero, and the reason an
/// empty mask means "Bluetooth is all of it" rather than "scan nothing".
const ASSIGN_BLE_ONLY: &[u8] = &[
    0x09, // epoch
    0x01, // ASSIGN_FLAG_BLE
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // no channels at all
];

/// Empty: the whole instruction is the type byte.
const CLEAR: &[u8] = &[];

/// An update session opening: session 7, a 0x0001_E240-byte image (123456), a hash of
/// 0x00..=0x1F, version 0.2.3, for an ESP32-C6.
const OTA_BEGIN: &[u8] = &[
    0x07, // session
    0x40, 0xE2, 0x01, 0x00, // len 123456, little-endian
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, // sha256
    0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, //
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, //
    0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F, //
    0x00, 0x02, 0x03, // version 0.2.3
    0x02, // board: ESP32-C6
];

/// A final chunk of three bytes. Data runs to the end of the body; nothing says its length.
const OTA_CHUNK_SHORT: &[u8] = &[
    0x07, // session
    0x00, 0x04, 0x01, 0x00, // offset 0x0001_0400, little-endian
    0xAA, 0xBB, 0xCC, // data
];

/// The session.
const OTA_COMMIT: &[u8] = &[0x07];

/// The session.
const OTA_ABORT: &[u8] = &[0x07];

/// Empty: the whole instruction is the type byte.
const OTA_CONFIRM: &[u8] = &[];

/// A node answering session 7: receiving, and wanting offset 0x0000_2000 next.
const OTA_STATUS: &[u8] = &[
    0x07, // session
    0x01, // state: receiving
    0x00, 0x20, 0x00, 0x00, // next_offset 8192, little-endian
];

#[test]
fn bus_msg_codec_matches_expected_body_lengths_when_checking_constants() {
    assert_eq!(HEARTBEAT.len(), HEARTBEAT_LEN);
    assert_eq!(ASSIGN.len(), ASSIGN_LEN);
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
    assert_eq!(CLEAR.len(), CLEAR_LEN, "empty");
    // The ceiling is the 1024 bytes every body a node sends keeps to, and a batch is packed
    // to fill exactly that many.
    const {
        assert!(HEARTBEAT_LEN < 32 && SIGHTING_RECORD_MAX < SIGHTING_BATCH_MAX);
        assert!(SIGHTING_BATCH_MAX == MAX_NODE_PAYLOAD);
    };
}

#[test]
fn heartbeat_serializes_byte_for_byte_when_encoded_and_decoded() {
    let msg = Heartbeat {
        capabilities: Capabilities {
            version: Version::new(0, 1, 2),
            board: Board::Esp32c5,
            five_ghz: true,
            trial: false,
        },
        mac: [0x02, 0, 0, 0, 0x0D, 0x20],
        counter: 0x1234_5678,
        epoch: 5,
        wifi_refused: 42,
        ble_refused: 0x0304,
        reset_cause: ResetCause::Brownout,
        vdd: Vdd(67),
        die_temp: DieTemp(81),
    };
    assert_eq!(msg.encode().as_slice(), HEARTBEAT);
    assert_eq!(Heartbeat::decode(HEARTBEAT), Ok(msg));
}

#[test]
fn heartbeat_serializes_epoch_zero_when_node_holds_no_assignment() {
    let msg = Heartbeat {
        capabilities: Capabilities {
            version: Version::new(0, 1, 0),
            board: Board::Esp32c5,
            five_ghz: false,
            trial: true,
        },
        mac: [0x02, 0, 0, 0, 0x1A, 0x50],
        counter: 0x1234_5678,
        epoch: 0,
        wifi_refused: 0,
        ble_refused: 0,
        reset_cause: ResetCause::PowerOn,
        vdd: Vdd::NONE,
        die_temp: DieTemp::NONE,
    };
    assert_eq!(msg.encode().as_slice(), HEARTBEAT_NO_EPOCH);
    assert_eq!(Heartbeat::decode(HEARTBEAT_NO_EPOCH), Ok(msg));
}

#[test]
fn heartbeat_carries_mac_after_prefix_when_encoded() {
    let decoded = Heartbeat::decode(HEARTBEAT).expect("valid");
    assert_eq!(decoded.mac, [0x02, 0, 0, 0, 0x0D, 0x20]);
    assert_eq!(&HEARTBEAT[HEARTBEAT_PREFIX_LEN..HEARTBEAT_IDENTITY_LEN], &decoded.mac);
}

#[test]
fn heartbeat_serializes_counter_as_little_endian_when_encoded() {
    // A counter read big-endian would still be monotonic, so the reboot
    // detector would never notice; only the bytes say which it is.
    let decoded = Heartbeat::decode(HEARTBEAT).expect("valid");
    assert_eq!(decoded.counter, 0x1234_5678);
    assert_eq!(&HEARTBEAT[11..15], &[0x78, 0x56, 0x34, 0x12]);
}

#[test]
fn heartbeat_serializes_refusal_counts_as_little_endian_when_encoded() {
    let decoded = Heartbeat::decode(HEARTBEAT).expect("valid");
    assert_eq!(decoded.wifi_refused, 42);
    assert_eq!(decoded.ble_refused, 0x0304);
    assert_eq!(&HEARTBEAT[16..18], &[0x2A, 0]);
    assert_eq!(&HEARTBEAT[18..20], &[0x04, 0x03]);
}

#[test]
fn heartbeat_round_trips_reset_cause_when_encoded_at_its_offset() {
    let decoded = Heartbeat::decode(HEARTBEAT).expect("valid");
    assert_eq!(decoded.reset_cause, ResetCause::Brownout);
    assert_eq!(HEARTBEAT[20], 0x05);

    for reset_cause in [
        ResetCause::PowerOn,
        ResetCause::Software,
        ResetCause::Watchdog,
        ResetCause::Lockup,
        ResetCause::Brownout,
        ResetCause::External,
        ResetCause::Unknown,
    ] {
        let msg = Heartbeat { reset_cause, ..decoded };
        let body = msg.encode();
        assert_eq!(body[20], reset_cause.as_u8());
        assert_eq!(Heartbeat::decode(&body), Ok(msg));
    }
}

#[test]
fn heartbeat_round_trips_health_when_encoded_at_its_offsets() {
    let decoded = Heartbeat::decode(HEARTBEAT).expect("valid");
    assert_eq!((decoded.vdd.mv(), decoded.die_temp.c()), (Some(3304), Some(41)));
    assert_eq!(&HEARTBEAT[21..23], &[0x43, 0x51]);

    let msg = Heartbeat { vdd: Vdd::NONE, die_temp: DieTemp::NONE, ..decoded };
    let body = msg.encode();
    assert_eq!(&body[21..23], &[0x00, 0xFF]);
    assert_eq!(Heartbeat::decode(&body), Ok(msg));
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
fn heartbeat_decodes_unknown_reset_cause_when_byte_unassigned() {
    // A node naming a cause this build has no name for is still a node.
    let mut body = HEARTBEAT.to_vec();
    body[20] = 0xEE;
    let decoded = Heartbeat::decode(&body).expect("valid");
    assert_eq!(decoded.reset_cause, ResetCause::Unknown);
}

#[test]
fn heartbeat_rejects_body_when_node_sends_shorter_heartbeat() {
    // A node flashed from another tree, whose heartbeat is shorter. The engine counts
    // this as incompatible and asks for a reflash.
    let got = HEARTBEAT_LEN - 2;
    assert_eq!(
        Heartbeat::decode(&HEARTBEAT[..got]),
        Err(DecodeError::BadLength { need: HEARTBEAT_LEN, got })
    );
    assert_eq!(
        UpMsg::decode(UpType::Heartbeat.as_u8(), &HEARTBEAT[..got]),
        Err(DecodeError::BadLength { need: HEARTBEAT_LEN, got })
    );
}

#[test]
fn heartbeat_rejects_body_when_node_sends_longer_heartbeat() {
    // A longer body is the dangerous half, because its leading bytes would parse.
    let mut longer = HEARTBEAT.to_vec();
    longer.push(0);
    assert_eq!(
        Heartbeat::decode(&longer),
        Err(DecodeError::BadLength { need: HEARTBEAT_LEN, got: HEARTBEAT_LEN + 1 })
    );
    assert_eq!(
        UpMsg::decode(UpType::Heartbeat.as_u8(), &longer),
        Err(DecodeError::BadLength { need: HEARTBEAT_LEN, got: HEARTBEAT_LEN + 1 })
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
    // out, so a batch from a build with a wider trailer is named rather than
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
    // Eighty-four bare records and one carrying a one-byte SSID sum to
    // exactly 1021 bytes behind the three-byte header: 1024 total, the batch
    // ceiling, reached at exactly SIGHTINGS_PER_BATCH_MAX records.
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
    assert!(writer.push(&SightingMsg { ssid: b"1", ..minimal }));
    assert_eq!(writer.len(), SIGHTINGS_PER_BATCH_MAX);
    assert_eq!(writer.as_bytes().len(), SIGHTING_BATCH_MAX);

    // One more record, however small, no longer fits: nothing is written, and
    // the batch already packed is unchanged.
    assert!(!writer.push(&minimal));
    assert_eq!(writer.len(), SIGHTINGS_PER_BATCH_MAX, "the refused push wrote nothing");
    assert_eq!(writer.as_bytes().len(), SIGHTING_BATCH_MAX);

    let batch = SightingBatch::decode(writer.as_bytes()).expect("valid");
    assert_eq!(batch.iter().count(), SIGHTINGS_PER_BATCH_MAX);
}

#[test]
fn sighting_batch_rejects_body_when_count_is_zero() {
    let mut body = SIGHTING_BATCH_1[..SIGHTING_BATCH_HEADER].to_vec();
    body[2] = 0; // count
    assert_eq!(
        SightingBatch::decode(&body),
        Err(DecodeError::TooShort {
            need: SIGHTING_BATCH_HEADER + SIGHTING_RECORD_MIN,
            got: body.len(),
        })
    );
}

#[test]
fn sighting_batch_rejects_body_when_count_exceeds_records_present() {
    let mut body = SIGHTING_BATCH_1.to_vec();
    body[2] = 2; // claims a second record that is not there
    assert_eq!(
        SightingBatch::decode(&body),
        Err(DecodeError::TooShort { need: SIGHTING_RECORD_MIN, got: 0 })
    );
}

#[test]
fn sighting_batch_rejects_body_when_trailing_bytes_follow_last_record() {
    let mut body = SIGHTING_BATCH_1.to_vec();
    body.extend_from_slice(BLE_RECORD); // count still says 1
    assert_eq!(
        SightingBatch::decode(&body),
        Err(DecodeError::BadLength {
            need: SIGHTING_BATCH_HEADER + WIFI_RECORD.len(),
            got: body.len(),
        })
    );
}

#[test]
fn sighting_batch_rejects_whole_body_when_second_record_is_malformed() {
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
fn assign_serializes_byte_for_byte_when_encoded_to_wire() {
    let msg = Assign { epoch: 7, flags: 0, channels: ChannelSet::from_run(IndexRun::new(16, 23)) };
    assert_eq!(msg.encode().as_slice(), ASSIGN);
    assert_eq!(Assign::decode(ASSIGN), Ok(msg));
}

#[test]
fn assign_serializes_channel_mask_as_little_endian_when_encoded() {
    // Four indices chosen to straddle byte boundaries and to reach the top of
    // the forty-two, so a mask written big-endian — or in five bytes rather
    // than six — cannot produce these bytes.
    let mut channels = ChannelSet::empty();
    for idx in [0, 6, 13, 41] {
        channels.insert(idx);
    }
    let msg = Assign { epoch: 200, flags: ASSIGN_FLAG_BLE, channels };
    assert_eq!(msg.encode().as_slice(), ASSIGN_BLE);

    let back = Assign::decode(ASSIGN_BLE).expect("valid");
    assert_eq!(back.channels.indices().collect::<Vec<_>>(), vec![0, 6, 13, 41]);
    assert!(back.scan_ble());
}

#[test]
fn assign_encodes_ble_flag_and_empty_mask_when_node_scans_bluetooth() {
    let msg = Assign { epoch: 9, flags: ASSIGN_FLAG_BLE, channels: ChannelSet::empty() };
    assert_eq!(msg.encode().as_slice(), ASSIGN_BLE_ONLY);

    let back = Assign::decode(ASSIGN_BLE_ONLY).expect("valid");
    assert!(back.channels.is_empty(), "it sniffs nothing");
    assert!(back.scan_ble(), "and the flag is what says why");
}

#[test]
fn assign_preserves_unknown_flag_bits_when_round_tripped() {
    // A newer host setting a bit this build has no name for must not have it
    // quietly dropped on the way through.
    let mut body = ASSIGN_BLE.to_vec();
    body[1] = 0b1000_0001;
    let decoded = Assign::decode(&body).expect("valid");
    assert_eq!(decoded.flags, 0b1000_0001);
    assert!(decoded.scan_ble());
    assert_eq!(decoded.encode().as_slice(), body.as_slice());
}

#[test]
fn clear_serializes_byte_for_byte_when_encoded_and_decoded() {
    assert_eq!(Clear.encode().as_slice(), CLEAR);
    assert_eq!(Clear::decode(CLEAR), Ok(Clear));
}

#[test]
fn clear_rejects_body_when_not_empty() {
    // The same reasoning `BadLength` exists for everywhere else: a wider body from a later
    // build must not be taken as a plausible clear.
    assert_eq!(Clear::decode(&[0xFF]), Err(DecodeError::BadLength { need: CLEAR_LEN, got: 1 }));
}

#[test]
fn msg_decoder_dispatches_type_byte_to_corresponding_message_when_decoded() {
    let up = |kind: UpType, body| UpMsg::decode(kind.as_u8(), body);
    let down = |kind: DownType, body| DownMsg::decode(kind.as_u8(), body);
    assert!(matches!(up(UpType::Heartbeat, HEARTBEAT), Ok(UpMsg::Heartbeat(_))));
    assert!(matches!(up(UpType::Sightings, SIGHTING_BATCH_1), Ok(UpMsg::Sightings(_))));
    assert!(matches!(up(UpType::Sightings, SIGHTING_BATCH_BLE), Ok(UpMsg::Sightings(_))));
    assert!(matches!(up(UpType::UpdateStatus, OTA_STATUS), Ok(UpMsg::UpdateStatus(_))));
    assert!(matches!(down(DownType::Assign, ASSIGN), Ok(DownMsg::Assign(_))));
    assert!(matches!(down(DownType::Clear, CLEAR), Ok(DownMsg::Clear(_))));
    assert!(matches!(down(DownType::UpdateBegin, OTA_BEGIN), Ok(DownMsg::UpdateBegin(_))));
    assert!(matches!(down(DownType::UpdateChunk, OTA_CHUNK_SHORT), Ok(DownMsg::UpdateChunk(_))));
    assert!(matches!(down(DownType::UpdateCommit, OTA_COMMIT), Ok(DownMsg::UpdateCommit(_))));
    assert!(matches!(down(DownType::UpdateAbort, OTA_ABORT), Ok(DownMsg::UpdateAbort(_))));
    assert!(matches!(down(DownType::UpdateConfirm, OTA_CONFIRM), Ok(DownMsg::UpdateConfirm(_))));
}

#[test]
fn msg_types_number_each_direction_from_one_when_pinned() {
    let up = [UpType::Heartbeat, UpType::Sightings, UpType::UpdateStatus];
    for (byte, kind) in (1..).zip(up) {
        assert_eq!(kind.as_u8(), byte);
        assert_eq!(UpType::from_u8(byte), Some(kind));
    }
    let down = [
        DownType::Assign,
        DownType::Clear,
        DownType::UpdateBegin,
        DownType::UpdateChunk,
        DownType::UpdateCommit,
        DownType::UpdateAbort,
        DownType::UpdateConfirm,
    ];
    for (byte, kind) in (1..).zip(down) {
        assert_eq!(kind.as_u8(), byte);
        assert_eq!(DownType::from_u8(byte), Some(kind));
    }
    assert_eq!(UpType::from_u8(0), None);
    assert_eq!(UpType::from_u8(4), None);
    assert_eq!(DownType::from_u8(8), None);
}

#[test]
fn down_msg_encodes_body_when_written_into_buffer() {
    let mut out = [0u8; MAX_NODE_DOWN];
    let msg = DownMsg::Assign(Assign::decode(ASSIGN).expect("valid"));
    assert_eq!(msg.kind(), DownType::Assign);
    let len = msg.encode_into(&mut out).expect("fits");
    assert_eq!(&out[..len], ASSIGN);
    let chunk = DownMsg::UpdateChunk(UpdateChunk::decode(OTA_CHUNK_SHORT).expect("valid"));
    let len = chunk.encode_into(&mut out).expect("fits");
    assert_eq!(&out[..len], OTA_CHUNK_SHORT);
    assert_eq!(DownMsg::Clear(Clear).encode_into(&mut out), Some(0));
}

#[test]
fn msg_decoder_rejects_body_when_type_does_not_fit_it() {
    // A record names its type, so a body read under the wrong one fails its length check:
    // reading an assignment out of a heartbeat would be adopting counter bytes as a mask.
    let down = |kind: DownType, body| DownMsg::decode(kind.as_u8(), body);
    let up = |kind: UpType, body| UpMsg::decode(kind.as_u8(), body);
    assert!(matches!(down(DownType::Assign, HEARTBEAT), Err(DecodeError::BadLength { .. })));
    assert!(matches!(up(UpType::Heartbeat, ASSIGN), Err(DecodeError::BadLength { .. })));
    assert!(matches!(down(DownType::UpdateBegin, OTA_COMMIT), Err(DecodeError::BadLength { .. })));
    assert!(matches!(
        down(DownType::UpdateConfirm, OTA_COMMIT),
        Err(DecodeError::BadLength { .. })
    ));
    assert!(matches!(up(UpType::UpdateStatus, OTA_COMMIT), Err(DecodeError::BadLength { .. })));
}

#[test]
fn msg_decoder_rejects_record_when_type_byte_is_unknown() {
    assert_eq!(UpMsg::decode(0x42, HEARTBEAT), Err(DecodeError::UnknownType(0x42)));
    assert_eq!(DownMsg::decode(0x42, ASSIGN), Err(DecodeError::UnknownType(0x42)));
}

#[test]
fn msg_decoder_rejects_truncated_body_when_body_is_too_short() {
    // A batch needs its own header: seq and count.
    assert_eq!(
        UpMsg::decode(UpType::Sightings.as_u8(), &SIGHTING_BATCH_1[..SIGHTING_BATCH_HEADER - 1]),
        Err(DecodeError::TooShort { need: SIGHTING_BATCH_HEADER, got: SIGHTING_BATCH_HEADER - 1 })
    );
    // The other two are each one size, so either side of it is a layout this
    // build does not read.
    assert_eq!(
        UpMsg::decode(UpType::Heartbeat.as_u8(), &HEARTBEAT[..HEARTBEAT_LEN - 1]),
        Err(DecodeError::BadLength { need: HEARTBEAT_LEN, got: HEARTBEAT_LEN - 1 })
    );
    assert_eq!(
        DownMsg::decode(DownType::Assign.as_u8(), &ASSIGN[..ASSIGN_LEN - 1]),
        Err(DecodeError::BadLength { need: ASSIGN_LEN, got: ASSIGN_LEN - 1 })
    );
}

#[test]
fn heartbeat_pins_prefix_and_mac_when_inspecting_body() {
    // Version, board and flags, then the MAC: what a host reads from a node on another
    // layout to name it.
    assert_eq!(HEARTBEAT_LEN, 23);
    assert_eq!(HEARTBEAT_PREFIX_LEN, 5);
    assert_eq!(HEARTBEAT_IDENTITY_LEN, 11);
    assert_eq!(&HEARTBEAT[..11], &[0, 1, 2, 1, 1, 0x02, 0, 0, 0, 0x0D, 0x20]);
}

#[test]
fn heartbeat_prefix_decodes_version_board_and_flags_when_body_is_exact() {
    let prefix = Capabilities::decode_prefix(HEARTBEAT).expect("a heartbeat");
    assert_eq!(prefix, Heartbeat::decode(HEARTBEAT).expect("valid").capabilities);
    assert_eq!(prefix.version, Version::new(0, 1, 2));
    assert_eq!(prefix.board, Board::Esp32c5);
}

#[test]
fn heartbeat_prefix_decodes_when_body_is_any_length_past_prefix() {
    // An older or newer minor's heartbeat: the prefix is all that is read.
    let older = [0, 0, 7, 2, 0];
    let caps = Capabilities::decode_prefix(&older).expect("prefix only");
    assert_eq!(caps.version, Version::new(0, 0, 7));
    assert_eq!(caps.board, Board::Esp32c6);
    assert!(!caps.five_ghz);

    let mut longer = HEARTBEAT.to_vec();
    longer.extend_from_slice(&[0xAA; 40]);
    assert_eq!(Capabilities::decode_prefix(&longer), Capabilities::decode_prefix(HEARTBEAT));
    assert_eq!(Heartbeat::identity(&longer), Heartbeat::identity(HEARTBEAT));
}

#[test]
fn heartbeat_prefix_rejects_body_when_shorter_than_prefix() {
    assert_eq!(
        Capabilities::decode_prefix(&HEARTBEAT[..4]),
        Err(DecodeError::TooShort { need: 5, got: 4 })
    );
    assert_eq!(
        Heartbeat::identity(&HEARTBEAT[..10]),
        Err(DecodeError::TooShort { need: 11, got: 10 })
    );
}

#[test]
fn msg_decoder_rejects_body_when_fixed_body_exceeds_expected_length() {
    // The half of a layout change that would otherwise be silent: a wider assignment from a
    // newer host has a known type byte and enough bytes for every field this build knows, so
    // a length check is the only thing between it and being adopted as a plausible wrong
    // share.
    let mut wider = ASSIGN.to_vec();
    wider.push(0xFF);
    assert_eq!(
        DownMsg::decode(DownType::Assign.as_u8(), &wider),
        Err(DecodeError::BadLength { need: ASSIGN_LEN, got: ASSIGN_LEN + 1 })
    );
}

#[test]
fn update_codec_matches_expected_body_lengths_when_checking_constants() {
    assert_eq!(OTA_BEGIN.len(), UPDATE_BEGIN_LEN);
    assert_eq!(UPDATE_BEGIN_LEN, 41);
    assert_eq!(UPDATE_CHUNK_HEADER, 5);
    assert_eq!(UPDATE_CHUNK, 1024);
    assert_eq!(UPDATE_CHUNK_MAX, 1029);
    assert_eq!(OTA_COMMIT.len(), UPDATE_COMMIT_LEN);
    assert_eq!(OTA_ABORT.len(), UPDATE_ABORT_LEN);
    assert_eq!(UPDATE_COMMIT_LEN, 1);
    assert_eq!(OTA_CONFIRM.len(), UPDATE_CONFIRM_LEN);
    assert_eq!(UPDATE_CONFIRM_LEN, 0);
    assert_eq!(OTA_STATUS.len(), UPDATE_STATUS_LEN);
    assert_eq!(UPDATE_STATUS_LEN, 6);
    // A chunk is longer than anything a node sends; the status keeps to the node ceiling.
    const {
        assert!(UPDATE_CHUNK_MAX == MAX_NODE_DOWN);
        assert!(UPDATE_CHUNK_MAX > MAX_NODE_PAYLOAD);
        assert!(UPDATE_STATUS_LEN <= MAX_NODE_PAYLOAD);
    };
}

fn ota_begin() -> UpdateBegin {
    let mut sha256 = [0u8; 32];
    for (i, byte) in sha256.iter_mut().enumerate() {
        *byte = i as u8;
    }
    UpdateBegin {
        session: 7,
        len: 123_456,
        sha256,
        version: Version::new(0, 2, 3),
        board: Board::Esp32c6,
    }
}

#[test]
fn update_begin_serializes_byte_for_byte_when_encoded_and_decoded() {
    assert_eq!(ota_begin().encode().as_slice(), OTA_BEGIN);
    assert_eq!(UpdateBegin::decode(OTA_BEGIN), Ok(ota_begin()));
}

#[test]
fn update_begin_rejects_body_when_length_differs() {
    let mut wider = OTA_BEGIN.to_vec();
    wider.push(0);
    assert_eq!(
        UpdateBegin::decode(&wider),
        Err(DecodeError::BadLength { need: UPDATE_BEGIN_LEN, got: UPDATE_BEGIN_LEN + 1 })
    );
    assert_eq!(
        UpdateBegin::decode(&OTA_BEGIN[..UPDATE_BEGIN_LEN - 1]),
        Err(DecodeError::BadLength { need: UPDATE_BEGIN_LEN, got: UPDATE_BEGIN_LEN - 1 })
    );
}

#[test]
fn update_chunk_serializes_byte_for_byte_when_encoded_and_decoded() {
    let chunk = UpdateChunk { session: 7, offset: 0x0001_0400, data: &[0xAA, 0xBB, 0xCC] };
    let mut out = [0u8; UPDATE_CHUNK_MAX];
    let len = chunk.encode_into(&mut out).expect("fits");
    assert_eq!(&out[..len], OTA_CHUNK_SHORT);
    assert_eq!(UpdateChunk::decode(OTA_CHUNK_SHORT), Ok(chunk));
}

#[test]
fn update_chunk_round_trips_when_data_is_a_full_chunk() {
    let data: Vec<u8> = (0..UPDATE_CHUNK).map(|i| i as u8).collect();
    let chunk = UpdateChunk { session: 1, offset: 4096, data: &data };
    let mut out = [0u8; UPDATE_CHUNK_MAX];
    assert_eq!(chunk.encode_into(&mut out), Some(UPDATE_CHUNK_MAX));
    assert_eq!(UpdateChunk::decode(&out), Ok(chunk));
}

#[test]
fn update_chunk_rejects_body_when_data_is_empty_or_longer_than_a_chunk() {
    assert_eq!(
        UpdateChunk::decode(&OTA_CHUNK_SHORT[..UPDATE_CHUNK_HEADER]),
        Err(DecodeError::TooShort { need: UPDATE_CHUNK_HEADER + 1, got: UPDATE_CHUNK_HEADER })
    );
    let mut wider = [0u8; UPDATE_CHUNK_MAX + 1];
    wider[..UPDATE_CHUNK_HEADER].copy_from_slice(&OTA_CHUNK_SHORT[..UPDATE_CHUNK_HEADER]);
    assert_eq!(
        UpdateChunk::decode(&wider),
        Err(DecodeError::BadLength { need: UPDATE_CHUNK_MAX, got: UPDATE_CHUNK_MAX + 1 })
    );
}

#[test]
fn update_chunk_refuses_to_encode_when_data_is_empty_or_too_long() {
    let mut out = [0x5Au8; UPDATE_CHUNK_MAX];
    let empty = UpdateChunk { session: 1, offset: 0, data: &[] };
    assert_eq!(empty.encode_into(&mut out), None);
    let long = [0u8; UPDATE_CHUNK + 1];
    let long = UpdateChunk { session: 1, offset: 0, data: &long };
    assert_eq!(long.encode_into(&mut out), None);
    let short_out = &mut [0u8; UPDATE_CHUNK_HEADER + 2];
    let three = UpdateChunk { session: 1, offset: 0, data: &[1, 2, 3] };
    assert_eq!(three.encode_into(short_out), None);
    assert!(out.iter().all(|&b| b == 0x5A), "nothing written on a refusal");
}

#[test]
fn update_commit_and_abort_serialize_byte_for_byte_when_encoded_and_decoded() {
    assert_eq!(UpdateCommit { session: 7 }.encode().as_slice(), OTA_COMMIT);
    assert_eq!(UpdateCommit::decode(OTA_COMMIT), Ok(UpdateCommit { session: 7 }));
    assert_eq!(UpdateAbort { session: 7 }.encode().as_slice(), OTA_ABORT);
    assert_eq!(UpdateAbort::decode(OTA_ABORT), Ok(UpdateAbort { session: 7 }));

    let mut wider = OTA_COMMIT.to_vec();
    wider.push(0);
    assert_eq!(
        UpdateCommit::decode(&wider),
        Err(DecodeError::BadLength { need: UPDATE_COMMIT_LEN, got: UPDATE_COMMIT_LEN + 1 })
    );
    assert_eq!(
        UpdateAbort::decode(&OTA_ABORT[..UPDATE_ABORT_LEN - 1]),
        Err(DecodeError::BadLength { need: UPDATE_ABORT_LEN, got: UPDATE_ABORT_LEN - 1 })
    );
}

#[test]
fn update_confirm_serializes_byte_for_byte_when_encoded_and_decoded() {
    assert_eq!(UpdateConfirm.encode().as_slice(), OTA_CONFIRM);
    assert_eq!(UpdateConfirm::decode(OTA_CONFIRM), Ok(UpdateConfirm));
    let mut wider = OTA_CONFIRM.to_vec();
    wider.push(0);
    assert_eq!(
        UpdateConfirm::decode(&wider),
        Err(DecodeError::BadLength { need: UPDATE_CONFIRM_LEN, got: UPDATE_CONFIRM_LEN + 1 })
    );
}

#[test]
fn update_status_serializes_byte_for_byte_when_encoded_and_decoded() {
    let status = UpdateStatus { session: 7, state: UpdateState::Receiving, next_offset: 8192 };
    assert_eq!(status.encode().as_slice(), OTA_STATUS);
    assert_eq!(UpdateStatus::decode(OTA_STATUS), Ok(status));
    let mut wider = OTA_STATUS.to_vec();
    wider.push(0);
    assert_eq!(
        UpdateStatus::decode(&wider),
        Err(DecodeError::BadLength { need: UPDATE_STATUS_LEN, got: UPDATE_STATUS_LEN + 1 })
    );
}

#[test]
fn update_state_round_trips_every_byte_when_converted() {
    let named = [
        (UpdateState::Idle, 0),
        (UpdateState::Receiving, 1),
        (UpdateState::Verified, 2),
        (UpdateState::HashMismatch, 3),
        (UpdateState::WrongBoard, 4),
        (UpdateState::TooLarge, 5),
        (UpdateState::FlashError, 6),
    ];
    for (state, byte) in named {
        assert_eq!(state.as_u8(), byte);
        assert_eq!(UpdateState::from_u8(byte), state);
    }
    for raw in 0..=u8::MAX {
        assert_eq!(UpdateState::from_u8(raw).as_u8(), raw);
    }
    assert_eq!(UpdateState::from_u8(7), UpdateState::Unknown(7));
}
