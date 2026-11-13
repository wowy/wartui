//! The Bluetooth transport's pure parts: naming, writes, and what a subscription admits.
//!
//! The radio itself is btleplug's, and has no fake: connecting, subscribing and the OS's errors
//! are checked on the bench.

use std::time::{Duration, Instant};

use wartui_bridge::LinkEvent;
use wartui_bridge::ble::{
    DownWriter, Heard, Inbound, advertised_name, dropped_as_unbonded, is_remembered, is_takeover,
    names_bridge, refused_for_bonding,
};
use wartui_bridge::serial::BridgeSpec;
use wartui_proto::bus::{NodeBoard, Slot};
use wartui_proto::link::{
    BridgeToHost, FrameAccumulator, FromNodeBody, HostToBridge, LogStr, LoopPhase, MAX_FRAME,
    MAX_HOST_FRAME, ShortStr, ToNodeBody, USB_TOOK_BRIDGE, decode_frame, encode_frame,
    encode_host_frame,
};
use wartui_proto::reset::ResetCause;

const BRIDGE: [u8; 6] = [0x02, 0x00, 0x5E, 0x10, 0x44, 0xC0];

fn spec(text: &str) -> BridgeSpec {
    text.parse().unwrap_or_else(|never| match never {})
}

fn frame(msg: &BridgeToHost) -> Vec<u8> {
    let mut buf = [0; MAX_FRAME];
    let len = encode_frame(msg, &mut buf).unwrap();
    buf[..len].to_vec()
}

fn ready(mac: [u8; 6], uptime_ms: u32) -> BridgeToHost {
    BridgeToHost::Ready {
        mac,
        fw_version: ShortStr::new(),
        reset_cause: ResetCause::PowerOn,
        last_phase: LoopPhase::Unknown,
        uptime_ms,
        node_board: NodeBoard::NoCarrier,
        radio: None,
        image: None,
    }
}

fn rx(rx_us: u32) -> BridgeToHost {
    let body = FromNodeBody::from_slice(b"body").unwrap();
    BridgeToHost::FromNode { slot: Slot::new(0, 0).unwrap(), rx_us, kind: 1, body }
}

fn takeover() -> BridgeToHost {
    BridgeToHost::Error { message: LogStr::try_from(USB_TOOK_BRIDGE).unwrap() }
}

fn host_frame(cmd: &HostToBridge) -> Vec<u8> {
    let mut buf = [0; MAX_HOST_FRAME];
    let len = encode_host_frame(cmd, &mut buf).unwrap();
    buf[..len].to_vec()
}

/// What the engine would see of `heard`.
fn events(heard: Vec<Heard>) -> Vec<LinkEvent> {
    heard
        .into_iter()
        .filter_map(|heard| match heard {
            Heard::Event(event) => Some(event),
            _ => None,
        })
        .collect()
}

#[test]
fn ble_name_carries_last_two_octets_when_bridge_is_named_by_address_or_tail() {
    assert_eq!(advertised_name(&spec("02:00:5E:10:44:C0")).as_deref(), Some("wartui 44:C0"));
    assert_eq!(advertised_name(&spec("44:C0")).as_deref(), Some("wartui 44:C0"));
    assert_eq!(advertised_name(&spec("10:44:C0")).as_deref(), Some("wartui 44:C0"));
}

#[test]
fn ble_name_is_none_when_spec_gives_fewer_than_two_octets_or_a_path() {
    assert_eq!(advertised_name(&spec("C0")), None);
    assert_eq!(advertised_name(&spec("/dev/ttyACM0")), None);
}

#[test]
fn ble_name_matches_bridge_when_spec_names_its_octets() {
    assert!(names_bridge(Some(&spec("44:C0")), "wartui 44:C0"));
    assert!(names_bridge(Some(&spec("02:00:5E:10:44:C0")), "wartui 44:C0"));
    assert!(names_bridge(Some(&spec("C0")), "wartui 44:C0"), "one octet matches the last");
    assert!(names_bridge(None, "wartui 96:C4"), "unnamed takes any bridge");
}

#[test]
fn ble_name_refuses_bridge_when_octets_differ_or_name_is_not_a_bridge() {
    assert!(!names_bridge(Some(&spec("44:C0")), "wartui 44:C1"));
    assert!(!names_bridge(None, "wartui"));
    assert!(!names_bridge(None, "Pixel 8"));
    assert!(!names_bridge(None, "wartui 4:C0"));
    assert!(!names_bridge(Some(&spec("/dev/ttyACM0")), "wartui 44:C0"));
}

#[test]
fn ble_writer_leads_first_frame_with_terminator_when_connection_is_new() {
    let mut writer = DownWriter::new(247);
    let first: Vec<u8> = writer.writes(&HostToBridge::GetStatus).unwrap().concat();
    let mut want = vec![0x00];
    want.extend(host_frame(&HostToBridge::GetStatus));
    assert_eq!(first, want);
    let again: Vec<u8> = writer.writes(&HostToBridge::GetStatus).unwrap().concat();
    assert_eq!(again, host_frame(&HostToBridge::GetStatus), "only the first");
}

#[test]
fn ble_writer_leads_again_when_a_new_connection_starts() {
    let mut writer = DownWriter::new(247);
    writer.writes(&HostToBridge::GetStatus).unwrap();
    let mut reconnected = DownWriter::new(247);
    assert_eq!(reconnected.writes(&HostToBridge::GetStatus).unwrap()[0][0], 0x00);
}

#[test]
fn ble_writer_splits_frame_into_mtu_less_header_when_frame_is_long() {
    let body = ToNodeBody::from_slice(&[0x5A; 1000]).unwrap();
    let cmd = HostToBridge::ToNode { slot: Slot::new(0, 0).unwrap(), id: 1, kind: 2, body };
    let mut writer = DownWriter::new(247);
    let writes = writer.writes(&cmd).unwrap();
    assert!(writes.len() > 1);
    assert!(writes.iter().all(|w| w.len() <= 244), "ATT MTU 247 less its 3-byte header");
    assert!(writes[..writes.len() - 1].iter().all(|w| w.len() == 244));
    // Reassembled as the P4 does, the writes are one frame.
    let mut acc = FrameAccumulator::<MAX_HOST_FRAME>::new();
    let mut decoded = None;
    for byte in writes.concat() {
        if let Some(frame) = acc.push(byte) {
            decoded = Some(decode_frame::<HostToBridge>(frame));
        }
    }
    assert_eq!(decoded, Some(Ok(cmd)));
}

#[test]
fn ble_writer_writes_twenty_bytes_when_mtu_is_the_default() {
    let body = ToNodeBody::from_slice(&[1; 64]).unwrap();
    let cmd = HostToBridge::ToNode { slot: Slot::new(0, 0).unwrap(), id: 1, kind: 2, body };
    let writes = DownWriter::new(23).writes(&cmd).unwrap();
    assert!(writes.iter().all(|w| w.len() <= 20));
    assert!(DownWriter::new(0).writes(&cmd).unwrap().iter().all(|w| w.len() <= 20));
}

#[test]
fn ble_notice_is_takeover_only_when_text_is_the_usb_notice() {
    assert!(is_takeover(&takeover()));
    let other = BridgeToHost::Error { message: LogStr::try_from("the radio stopped").unwrap() };
    assert!(!is_takeover(&other));
    let log = BridgeToHost::Log { message: LogStr::try_from(USB_TOOK_BRIDGE).unwrap() };
    assert!(!is_takeover(&log), "only as an Error");
}

#[test]
fn ble_inbound_drops_frames_when_they_arrive_before_first_ready() {
    let mut bytes = frame(&rx(10));
    bytes.extend(frame(&ready(BRIDGE, 500)));
    bytes.extend(frame(&rx(20)));
    let mut inbound = Inbound::new(None);
    let heard = inbound.feed(&bytes, Instant::now());
    assert_eq!(heard[0], Heard::Proved(BRIDGE), "remembered before the engine hears of it");
    let events = events(heard);
    assert!(matches!(events[0], LinkEvent::Connected(_)));
    assert!(matches!(events[1], LinkEvent::Message(BridgeToHost::FromNode { rx_us: 20, .. })));
    assert_eq!(events.len(), 2);
}

#[test]
fn ble_inbound_reassembles_frames_when_split_across_notifications() {
    let mut bytes = frame(&ready(BRIDGE, 500));
    bytes.extend(frame(&rx(20)));
    let mut inbound = Inbound::new(None);
    let now = Instant::now();
    let heard: Vec<Heard> = bytes.chunks(7).flat_map(|chunk| inbound.feed(chunk, now)).collect();
    assert_eq!(events(heard).len(), 2);
}

#[test]
fn ble_inbound_reports_disconnect_then_same_life_when_usb_takes_and_returns_bridge() {
    let mut inbound = Inbound::new(None);
    let t0 = Instant::now();
    inbound.feed(&frame(&ready(BRIDGE, 10_000)), t0);

    let gone = events(inbound.feed(&frame(&takeover()), t0 + Duration::from_secs(1)));
    assert_eq!(gone, [LinkEvent::Disconnected { reason: USB_TOOK_BRIDGE.to_owned() }]);
    assert!(!inbound.announced(), "subscribed, waiting for the handback");

    // Anything ahead of the handback's `Ready` is not this stream's.
    let mut bytes = frame(&rx(30));
    bytes.extend(frame(&ready(BRIDGE, 30_000)));
    let heard = inbound.feed(&bytes, t0 + Duration::from_secs(20));
    assert!(!heard.contains(&Heard::Proved(BRIDGE)), "proved once per connection");
    let back = events(heard);
    assert_eq!(back.len(), 1);
    assert!(matches!(&back[0], LinkEvent::Connected(info) if info.uptime_ms == 30_000));
}

#[test]
fn ble_inbound_refuses_bridge_when_it_shares_only_the_advertised_name() {
    let other = [0x02, 0xF2, 0x62, 0x10, 0x44, 0xC0];
    let mut inbound = Inbound::new(Some(spec("02:00:5E:10:44:C0")));
    let heard = inbound.feed(&frame(&ready(other, 500)), Instant::now());
    assert_eq!(heard, [Heard::WrongBridge(other)]);
}

#[test]
fn ble_failure_reads_as_not_bonded_when_os_reports_att_security_error() {
    for error in [
        "org.bluez.Error.NotPermitted: Write not permitted",
        "org.bluez.Error.Failed: Operation failed with ATT error: 0x05",
        "Insufficient Encryption",
        "org.bluez.Error.NotAuthorized: Not Authorized",
    ] {
        assert!(refused_for_bonding(error), "{error}");
    }
    assert!(!refused_for_bonding("org.bluez.Error.Failed: le-connection-abort-by-local"));
    assert!(!refused_for_bonding("Timed out after 10s"));
}

#[test]
fn ble_drop_reads_as_not_bonded_when_bridge_hangs_up_about_ten_seconds_in_with_no_ready() {
    assert!(dropped_as_unbonded(Duration::from_secs(10), false));
    assert!(dropped_as_unbonded(Duration::from_secs(30), false), "inside the pairing window");
    assert!(!dropped_as_unbonded(Duration::from_secs(10), true), "a bridge that announced");
    assert!(!dropped_as_unbonded(Duration::from_secs(1), false), "a connect that failed");
    assert!(!dropped_as_unbonded(Duration::from_secs(120), false));
}

#[test]
fn ble_remembered_matches_address_or_identifier_when_case_differs() {
    assert!(is_remembered("02:00:5e:10:44:c1", "02:00:5E:10:44:C1", "hci0/dev_02_00_5E_10_44_C1"));
    let mac_id = "5c3e1a2b-0000-4000-8000-00805f9b34fb";
    assert!(is_remembered(mac_id, "00:00:00:00:00:00", mac_id), "macOS hides the address");
    assert!(!is_remembered("02:00:5E:10:44:C1", "02:00:5E:10:44:C2", "hci0/dev_02_00_5E_10_44_C2"));
}
