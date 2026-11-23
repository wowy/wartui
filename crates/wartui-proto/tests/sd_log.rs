//! The SD log's records: their framing, each kind's payload, and what a reader makes of a torn or
//! foreign file.

use heapless::String;
use wartui_proto::ChannelPool;
use wartui_proto::bus::{NodeBoard, Slot};
use wartui_proto::crc::crc32;
use wartui_proto::fleet::{Report, ScanMode};
use wartui_proto::host_owner::{Holder, Input};
use wartui_proto::link::{
    BridgeToHost, GpsBytes, LinkError, LogStr, LoopPhase, MAX_FRAME, PowerOutcome, SdFormatOutcome,
    Undelivered, UpdateStatusBytes, decode_frame, encode_frame,
};
use wartui_proto::plan::ChannelSet;
use wartui_proto::reset::ResetCause;
use wartui_proto::sd::log::{
    Bad, Body, Clock, CloseReason, Entry, FORMAT_VERSION, Kind, Opened, PAYLOAD_MAX, RECORD_MAX,
    decode, encode, encode_closed, encode_dropped, encode_message, encode_opened, encode_owner,
    encode_phone_line, logged, records,
};
use wartui_proto::wardrive_esp::session::Class;

#[path = "common/status.rs"]
mod status;
use status::largest_status;

fn opened() -> Opened {
    Opened {
        version: FORMAT_VERSION,
        file: 123,
        part: 0,
        opened_ms: 1000,
        fw_version: String::try_from("0.1.0").expect("short"),
        reset_cause: ResetCause::PowerOn,
        last_phase: LoopPhase::Boot,
        slot: String::try_from("ota_0").expect("short"),
        trial: false,
        image: None,
        card_mb: 30_436,
        free_mb: Some(30_000),
    }
}

fn log_msg(text: &str) -> BridgeToHost {
    BridgeToHost::Log { message: LogStr::try_from(text).expect("short") }
}

/// A record written by `write` into a buffer of [`RECORD_MAX`], as a file holds it.
fn record(write: impl FnOnce(&mut [u8]) -> Option<usize>) -> Vec<u8> {
    let mut out = vec![0; RECORD_MAX];
    let n = write(&mut out).expect("encodes");
    out.truncate(n);
    assert_eq!(out.last(), Some(&0), "ends in its terminator");
    assert!(!out[..n - 1].contains(&0), "holds no zero before it");
    out
}

/// Every record of `file`, decoded, with the kind and uptime of each.
fn read(file: &[u8], mut each: impl FnMut(Result<Entry<'_>, Bad>)) {
    let mut file = file.to_vec();
    let ranges: Vec<_> = records(&file).collect();
    for range in ranges {
        each(decode(&mut file[range]));
    }
}

/// The one record of `bytes`, decoded, its body printed to compare outside the borrow.
fn decoded(bytes: &[u8]) -> (u8, u32, std::string::String) {
    let mut seen = None;
    read(bytes, |entry| {
        assert!(seen.is_none(), "one record");
        let entry = entry.expect("decodes");
        seen = Some((entry.kind, entry.ms, debug(&entry.body)));
    });
    seen.expect("a record")
}

fn debug(body: &Body<'_>) -> std::string::String {
    format!("{body:?}")
}

#[test]
fn sd_log_round_trips_record_when_each_kind() {
    let status = largest_status();
    let cases: Vec<(Kind, Vec<u8>, Body<'_>)> = vec![
        (Kind::Opened, record(|o| encode_opened(&opened(), 7, o)), Body::Opened(opened())),
        (
            Kind::Frame,
            record(|o| encode_message(Kind::Frame, &log_msg("hi"), 7, o)),
            Body::Frame(log_msg("hi")),
        ),
        (
            Kind::Snapshot,
            record(|o| encode_message(Kind::Snapshot, &status, 7, o)),
            Body::Snapshot(status.clone()),
        ),
        (
            Kind::PhoneLine,
            record(|o| encode_phone_line(Class::Answer, b"WD:OK", 7, o)),
            Body::PhoneLine { class: Class::Answer, line: b"WD:OK" },
        ),
        (
            Kind::PhoneLine,
            record(|o| encode_phone_line(Class::Stream, b"", 7, o)),
            Body::PhoneLine { class: Class::Stream, line: b"" },
        ),
        (
            Kind::PhoneCommand,
            record(|o| encode(Kind::PhoneCommand, 7, b"WD:MODE WIFI", o)),
            Body::PhoneCommand(b"WD:MODE WIFI"),
        ),
        (
            Kind::Owner,
            record(|o| encode_owner(Input::PhoneSubscribed, Holder::None, Holder::Phone, 7, o)),
            Body::Owner {
                input: Input::PhoneSubscribed,
                before: Holder::None,
                after: Holder::Phone,
            },
        ),
        (Kind::Dropped, record(|o| encode_dropped(u32::MAX, 7, o)), Body::Dropped(u32::MAX)),
        (
            Kind::Closed,
            record(|o| encode_closed(CloseReason::SettingOff, 7, o)),
            Body::Closed(CloseReason::SettingOff),
        ),
        (
            Kind::Closed,
            record(|o| encode_closed(CloseReason::Rotation, 7, o)),
            Body::Closed(CloseReason::Rotation),
        ),
    ];
    for (kind, bytes, want) in &cases {
        let (code, ms, body) = decoded(bytes);
        assert_eq!((code, ms), (kind.code(), 7), "{kind:?}");
        assert_eq!(body, debug(want), "{kind:?}");
    }
    // The same bodies compare equal, not only print alike.
    let mut bytes = record(|o| encode_message(Kind::Snapshot, &status, 7, o));
    let len = bytes.len() - 1;
    assert_eq!(decode(&mut bytes[..len]).expect("decodes").body, Body::Snapshot(status));
}

#[test]
fn sd_log_numbers_kinds_when_coded() {
    let kinds = [
        Kind::Opened,
        Kind::Frame,
        Kind::Snapshot,
        Kind::PhoneLine,
        Kind::PhoneCommand,
        Kind::Owner,
        Kind::Dropped,
        Kind::Closed,
    ];
    for (code, kind) in (1..).zip(kinds) {
        assert_eq!(kind.code(), code);
        assert_eq!(Kind::from_code(code), Some(kind));
    }
    assert_eq!(Kind::from_code(0), None);
    assert_eq!(Kind::from_code(9), None);
}

#[test]
fn sd_log_reads_unknown_kind_when_crc_matches() {
    // A record of a kind this build does not know: framed and checked like any other.
    let mut body = vec![42, 1, 0, 0, 0, b'x', b'y'];
    body.extend_from_slice(&crc32(&body).to_le_bytes());
    let mut bytes = vec![0; RECORD_MAX];
    let n = cobs::encode(&body, &mut bytes);
    let entry = decode(&mut bytes[..n]).expect("decodes");
    assert_eq!((entry.kind, entry.ms), (42, 1));
    assert_eq!(entry.body, Body::Unknown { kind: 42, bytes: b"xy" });
}

#[test]
fn sd_log_frame_payload_decodes_with_decode_frame_when_recorded() {
    let msg = BridgeToHost::Fleet(Report {
        mode: ScanMode::Wifi,
        pool: ChannelPool::Eu,
        shares: heapless::Vec::new(),
        unreachable: ChannelSet::empty(),
    });
    let mut frame = [0; MAX_FRAME];
    let n = encode_frame(&msg, &mut frame).expect("encodes");
    let bytes = record(|o| encode(Kind::Frame, 99, &frame[..n - 1], o));
    assert_eq!(bytes, record(|o| encode_message(Kind::Frame, &msg, 99, o)), "one encoding");

    // The payload, cut out of the record by hand, is the frame byte for byte.
    let mut body = bytes[..bytes.len() - 1].to_vec();
    let len = cobs::decode_in_place(&mut body).expect("COBS");
    let payload = &mut body[5..len - 4];
    assert_eq!(payload, &frame[..n - 1]);
    assert_eq!(decode_frame::<BridgeToHost>(payload), Ok(msg));
}

#[test]
fn sd_log_reports_crc_when_byte_flipped_and_next_record_decodes() {
    let first = record(|o| encode(Kind::PhoneCommand, 0x0102_0304, b"hello", o));
    let second = record(|o| encode_dropped(3, 0x0102_0305, o));
    let mut file = [first.clone(), first, second].concat();
    // The code byte, the kind and the uptime, then `hello`: flip a bit of its `l`.
    file[1 + 5 + 2] ^= 0x20;
    let mut seen = Vec::new();
    read(&file, |entry| seen.push(entry.map(|e| debug(&e.body))));
    assert_eq!(
        seen,
        [Err(Bad::Crc), Ok(debug(&Body::PhoneCommand(b"hello"))), Ok(debug(&Body::Dropped(3)))]
    );
}

#[test]
fn sd_log_skips_zero_padding_when_file_holds_runs_of_zeros() {
    let one = record(|o| encode_dropped(1, 10, o));
    let two = record(|o| encode_dropped(2, 20, o));
    let file = [vec![0; 3], one.clone(), vec![0; 509], two.clone(), vec![0; 40]].concat();
    let ranges: Vec<_> = records(&file).collect();
    let starts = [3, 3 + one.len() + 509];
    let want: Vec<_> = starts.iter().zip([&one, &two]).map(|(&s, r)| s..s + r.len() - 1).collect();
    assert_eq!(ranges, want);
    assert_eq!(records(&[0; 512]).count(), 0, "a zeroed block holds nothing");
    assert_eq!(records(&[]).count(), 0);
}

#[test]
fn sd_log_flags_truncated_tail_when_file_is_cut() {
    let whole = record(|o| encode_opened(&opened(), 1000, o));
    let cut = record(|o| encode_message(Kind::Snapshot, &largest_status(), 1001, o));
    for keep in 1..cut.len() - 1 {
        let file = [&whole[..], &cut[..keep]].concat();
        let mut seen = Vec::new();
        read(&file, |entry| seen.push(entry.map(|e| e.kind)));
        assert_eq!(seen.len(), 2, "kept {keep}");
        assert_eq!(seen[0], Ok(Kind::Opened.code()), "kept {keep}");
        assert!(seen[1].is_err(), "kept {keep}: {:?}", seen[1]);
    }
}

#[test]
fn sd_log_reports_short_when_record_lacks_header() {
    for len in 0..9 {
        let body = vec![1; len];
        let mut bytes = vec![0; 16];
        let n = cobs::encode(&body, &mut bytes);
        assert_eq!(decode(&mut bytes[..n]), Err(Bad::Short), "{len} bytes");
    }
}

#[test]
fn sd_log_reports_cobs_when_code_runs_past_record() {
    // A code byte that promises more bytes than follow.
    assert_eq!(decode(&mut [9, 1, 1]), Err(Bad::Cobs));
}

#[test]
fn sd_log_reports_frame_error_when_frame_does_not_decode() {
    // Bytes that COBS-decode but fail the frame's own CRC.
    let mut bytes = record(|o| encode(Kind::Frame, 0, &[4, 1, 2, 3], o));
    let len = bytes.len() - 1;
    assert!(matches!(decode(&mut bytes[..len]), Err(Bad::Frame(LinkError::BadChecksum { .. }))));
    let mut bytes = record(|o| encode(Kind::Snapshot, 0, &[], o));
    let len = bytes.len() - 1;
    assert_eq!(decode(&mut bytes[..len]), Err(Bad::Frame(LinkError::Corrupt)), "no frame at all");
}

#[test]
fn sd_log_reports_malformed_when_payload_is_not_its_kinds() {
    let mut owner = [0; 4];
    let owned = postcard::to_slice(&(Input::UartFrame, Holder::Usb, Holder::Usb), &mut owner)
        .expect("fits")
        .len();
    let mut opened_v2 = vec![0; 256];
    let n =
        postcard::to_slice(&Opened { version: 2, ..opened() }, &mut opened_v2).expect("fits").len();
    opened_v2.truncate(n);
    let mut opened_long = vec![0; 256];
    let n = postcard::to_slice(&opened(), &mut opened_long).expect("fits").len();
    opened_long.truncate(n + 1);
    let cases: [(Kind, &[u8], &str); 11] = [
        (Kind::Opened, &opened_v2, "another format version"),
        (Kind::Opened, &opened_long, "a byte past the header"),
        (Kind::Opened, &[], "no header"),
        (Kind::PhoneLine, &[], "no class"),
        (Kind::PhoneLine, &[2, b'x'], "an unknown class"),
        (Kind::Owner, &owner[..owned - 1], "a holder short"),
        (Kind::Owner, &[3, 1, 1, 0], "a byte past the holders"),
        (Kind::Dropped, &[1, 2, 3], "three bytes"),
        (Kind::Dropped, &[1, 2, 3, 4, 5], "five bytes"),
        (Kind::Closed, &[2], "an unknown reason"),
        (Kind::Closed, &[0, 0], "two bytes"),
    ];
    for (kind, payload, why) in cases {
        let mut bytes = record(|o| encode(kind, 0, payload, o));
        let len = bytes.len() - 1;
        assert_eq!(decode(&mut bytes[..len]), Err(Bad::Malformed), "{kind:?}: {why}");
    }
}

#[test]
fn sd_log_logged_pins_each_variant_when_bridge_sends_it() {
    let status = UpdateStatusBytes::from_slice(&[1, 2]).expect("fits");
    let cases = [
        (
            BridgeToHost::Ready {
                mac: [0x02, 0, 0, 0, 0, 1],
                fw_version: String::new(),
                reset_cause: ResetCause::PowerOn,
                last_phase: LoopPhase::Unknown,
                uptime_ms: 0,
                node_board: NodeBoard::NoCarrier,
                radio: None,
                image: None,
            },
            true,
        ),
        (largest_status(), true),
        (log_msg("a log line"), true),
        (BridgeToHost::Error { message: LogStr::try_from("no").expect("short") }, true),
        (BridgeToHost::Gps { bytes: GpsBytes::from_slice(b"$GP").expect("fits") }, false),
        (BridgeToHost::Update(status.clone()), true),
        (
            BridgeToHost::FromNode {
                slot: Slot::new(0, 0).expect("A1"),
                rx_us: 0,
                kind: 1,
                body: heapless::Vec::new(),
            },
            false,
        ),
        (BridgeToHost::Delivered { id: 1 }, false),
        (BridgeToHost::Undelivered { id: 1, reason: Undelivered::Timeout }, false),
        (
            BridgeToHost::PowerCycled {
                slot: Slot::new(0, 0).expect("A1"),
                outcome: PowerOutcome::Done,
            },
            true,
        ),
        (BridgeToHost::CacheImage(status), true),
        (
            BridgeToHost::Fleet(Report {
                mode: ScanMode::Stopped,
                pool: ChannelPool::All,
                shares: heapless::Vec::new(),
                unreachable: ChannelSet::empty(),
            }),
            true,
        ),
        (BridgeToHost::SdFormatted { outcome: SdFormatOutcome::Done }, true),
    ];
    for (msg, want) in &cases {
        assert_eq!(logged(msg), *want, "{msg:?}");
    }
}

#[test]
fn sd_log_fits_record_when_status_is_largest() {
    let status = largest_status();
    let mut out = [0; RECORD_MAX];
    let n = encode_message(Kind::Snapshot, &status, u32::MAX, &mut out).expect("fits a record");
    assert!(n <= RECORD_MAX);
    let entry = decode(&mut out[..n - 1]).expect("decodes");
    assert_eq!(entry.body, Body::Snapshot(status));
}

#[test]
fn sd_log_refuses_record_when_payload_or_buffer_is_too_long_or_short() {
    // No zeros anywhere costs COBS the most.
    let payload = vec![0xFF; PAYLOAD_MAX];
    let mut out = vec![0; RECORD_MAX];
    let n = encode(Kind::PhoneCommand, u32::MAX, &payload, &mut out).expect("the longest fits");
    assert!(n <= RECORD_MAX, "{n}");
    assert_eq!(encode(Kind::PhoneCommand, 0, &vec![1; PAYLOAD_MAX + 1], &mut out), None);
    assert_eq!(encode(Kind::PhoneCommand, u32::MAX, &payload, &mut out[..n - 1]), None);
    assert_eq!(encode_dropped(1, 0, &mut []), None);
}

#[test]
fn sd_log_clock_unwraps_uptime_when_value_wraps_past_2_32() {
    let lap = 1u64 << 32;
    let mut clock = Clock::new(lap - 1000);
    assert_eq!(clock.at(u32::MAX - 500), lap - 501);
    assert_eq!(clock.at(100), lap + 100, "past the wrap");
    assert_eq!(clock.at(50), lap + 50, "a little back is not another lap");
    assert_eq!(clock.at(u32::MAX - 10), lap - 11, "back across the wrap");
    assert_eq!(clock.at(7), lap + 7);

    // A backlog older than the file's `Opened`.
    let mut clock = Clock::new(5_000);
    assert_eq!(clock.at(1_000), 1_000);
    assert_eq!(clock.at(6_000), 6_000);
    // Nothing comes before boot, so a step back past 0 stays on the first lap.
    let mut clock = Clock::new(10);
    assert_eq!(clock.at(u32::MAX - 5), u64::from(u32::MAX - 5));

    let mut clock = Clock::new(3 * lap + 7);
    assert_eq!(clock.at(9), 3 * lap + 9);
    assert_eq!(clock.at(9 + i32::MAX as u32), 3 * lap + 9 + i32::MAX as u64, "2^31 - 1 forward");
    assert_eq!(clock.at(9), 3 * lap + 9, "and as far back");
}

/// The record's body: COBS undone and the terminator dropped.
fn body_of(record: &[u8]) -> Vec<u8> {
    let mut body = record[..record.len() - 1].to_vec();
    let len = cobs::decode_in_place(&mut body).expect("COBS");
    body.truncate(len);
    body
}

#[test]
fn sd_log_pins_bytes_when_opened_recorded() {
    let bytes = record(|o| encode_opened(&opened(), 0x0102_0304, o));
    #[rustfmt::skip]
    let want = [
        1,                                    // kind: Opened
        0x04, 0x03, 0x02, 0x01,               // uptime_ms
        1,                                    // version
        123,                                  // file
        0,                                    // part
        0xE8, 0x07,                           // opened_ms: 1000
        5, b'0', b'.', b'1', b'.', b'0',      // fw_version
        0,                                    // reset_cause: PowerOn
        1,                                    // last_phase: Boot
        5, b'o', b't', b'a', b'_', b'0',      // slot
        0,                                    // trial
        0,                                    // image: None
        0xE4, 0xED, 0x01,                     // card_mb: 30,436
        1, 0xB0, 0xEA, 0x01,                  // free_mb: Some(30,000)
    ];
    let body = body_of(&bytes);
    assert_eq!(body[..body.len() - 4], want);
    assert_eq!(body[body.len() - 4..], crc32(&want).to_le_bytes());
}

#[test]
fn sd_log_pins_bytes_when_owner_recorded() {
    let bytes = record(|o| encode_owner(Input::UartFrame, Holder::Bluetooth, Holder::Usb, 9, o));
    // The kind, the uptime, then each variant's index.
    let want = [6, 9, 0, 0, 0, 3, 2, 1];
    let crc = crc32(&want).to_le_bytes();
    assert_eq!(body_of(&bytes), [&want[..], &crc].concat());
    // COBS replaces each zero with the distance to the next.
    assert_eq!(bytes[..4], [3, 6, 9, 1]);
}

#[test]
fn sd_log_never_panics_when_bytes_are_random() {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for round in 0..20_000 {
        let len = (next() % 300) as usize;
        let mut bytes: Vec<u8> = (0..len).map(|_| next() as u8).collect();
        // Straight into the decoder, zeros and all.
        let _ = decode(&mut bytes.clone());
        // As a file.
        read(&bytes, |entry| drop(entry));
        // As a payload under a good header and CRC, so every kind's reader sees it.
        let kind = Kind::from_code(1 + (round % 8) as u8).expect("a kind");
        bytes.truncate(PAYLOAD_MAX);
        let mut out = vec![0; RECORD_MAX];
        let n = encode(kind, next() as u32, &bytes, &mut out).expect("fits");
        let _ = decode(&mut out[..n - 1]);
    }
}
