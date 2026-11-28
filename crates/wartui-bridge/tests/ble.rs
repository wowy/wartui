//! The Bluetooth transport's pure parts: matching the advertised id, where it looks, writes, and
//! what a subscription admits. The desktop adapter's reading of the OS's words is
//! `wartui-desktop`'s (`crates/wartui-desktop/tests/radio.rs`).
//!
//! The driver's finding, connecting and serving run against a fake radio in its unit tests
//! (`src/ble/link_tests.rs`). The btleplug adapter itself is checked on the bench.

use std::time::{Duration, Instant};

use wartui_bridge::ble::{
    Advertiser, DownWrite, DownWriter, Heard, Inbound, Look, WriteKind, choose,
    dropped_as_unbonded, is_takeover, look, matches_octets, named_octets, settled_spec,
};
use wartui_bridge::spec::BridgeSpec;
use wartui_bridge::{DisconnectCause, LinkEvent, TransportError};
use wartui_proto::ble::{ATT_MTU, DOWN_VALUE_MAX};
use wartui_proto::bus::{NodeBoard, Slot, Version};
use wartui_proto::link::{
    BRIDGE_VERSION, BridgeToHost, End, FrameAccumulator, FromNodeBody, HostToBridge, LogStr,
    LoopPhase, MAX_FRAME, MAX_HOST_FRAME, Ready, ReadyCore, ReadyRest, ShortStr, Standing,
    ToNodeBody, Trial, USB_TOOK_BRIDGE, decode_host_frame, encode_frame, encode_host_frame,
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

fn ready_on(version: Version, mac: [u8; 6], uptime_ms: u32) -> BridgeToHost {
    BridgeToHost::Ready(Ready {
        core: ReadyCore { mac, image: [0; 32], version, trial: Trial::Settled, uptime_ms },
        rest: Some(ReadyRest {
            fw_version: ShortStr::new(),
            reset_cause: ResetCause::PowerOn,
            last_phase: LoopPhase::Unknown,
            node_board: NodeBoard::NoCarrier,
            radio: None,
        }),
    })
}

fn ready(mac: [u8; 6], uptime_ms: u32) -> BridgeToHost {
    ready_on(BRIDGE_VERSION, mac, uptime_ms)
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
fn ble_octets_are_last_two_when_bridge_is_named_by_address_or_tail() {
    assert_eq!(named_octets(&spec("02:00:5E:10:44:C0")), Some([0x44, 0xC0]));
    assert_eq!(named_octets(&spec("44:C0")), Some([0x44, 0xC0]));
    assert_eq!(named_octets(&spec("10:44:C0")), Some([0x44, 0xC0]));
}

#[test]
fn ble_octets_are_none_when_spec_gives_fewer_than_two_octets_or_a_path() {
    assert_eq!(named_octets(&spec("C0")), None);
    assert_eq!(named_octets(&spec("/dev/ttyACM0")), None);
}

#[test]
fn ble_octets_match_bridge_when_spec_names_them() {
    assert!(matches_octets(Some(&spec("44:C0")), [0x44, 0xC0]));
    assert!(matches_octets(Some(&spec("02:00:5E:10:44:C0")), [0x44, 0xC0]));
    assert!(matches_octets(Some(&spec("C0")), [0x44, 0xC0]), "one octet matches the last");
    assert!(matches_octets(None, [0x96, 0xC4]), "no spec takes any bridge");
}

#[test]
fn ble_octets_refuse_bridge_when_they_differ_or_spec_is_a_path() {
    assert!(!matches_octets(Some(&spec("44:C0")), [0x44, 0xC1]));
    assert!(!matches_octets(Some(&spec("C1")), [0x44, 0xC0]));
    assert!(!matches_octets(Some(&spec("/dev/ttyACM0")), [0x44, 0xC0]));
}

#[test]
fn ble_advertiser_label_is_name_when_os_heard_one_else_its_octets() {
    let named = Advertiser {
        octets: [0x44, 0xC0],
        name: Some("wartui 44:C0".to_owned()),
        address: ADDRESS.to_owned(),
    };
    assert_eq!(named.label(), "wartui 44:C0");
    let unnamed = Advertiser { name: None, ..named };
    assert_eq!(unnamed.label(), "bridge 44:C0");
}

/// A `ToNode` whose body is `len` bytes.
fn to_node(len: usize) -> HostToBridge {
    let body = ToNodeBody::from_slice(&vec![0x5A; len]).unwrap();
    HostToBridge::ToNode { slot: Slot::new(0, 0).unwrap(), id: 1, kind: 2, body }
}

/// The writes' values, end to end.
fn bytes(writes: &[DownWrite]) -> Vec<u8> {
    writes.iter().flat_map(|w| w.bytes.iter().copied()).collect()
}

/// The writes that carry `cmd` alone.
fn alone(writer: &mut DownWriter, cmd: &HostToBridge) -> Vec<DownWrite> {
    assert!(writer.offer(cmd).unwrap(), "an empty group takes any frame");
    writer.take()
}

/// The frames `writes` carry, reassembled as the P4 does.
fn frames(writes: &[DownWrite]) -> Vec<HostToBridge> {
    let mut acc = FrameAccumulator::<MAX_HOST_FRAME>::new();
    bytes(writes)
        .into_iter()
        .filter_map(|byte| acc.push(byte).map(|f| decode_host_frame(f).unwrap().unwrap()))
        .collect()
}

/// How many of `writes` are requests.
fn requests(writes: &[DownWrite]) -> usize {
    writes.iter().filter(|w| w.kind == WriteKind::Request).count()
}

/// The bytes of `cmd`'s frame behind the first group's leading `0x00`.
fn led(cmd: &HostToBridge) -> Vec<u8> {
    let mut want = vec![0x00];
    want.extend(host_frame(cmd));
    want
}

#[test]
fn ble_writer_leads_first_group_with_zero_byte_when_connection_is_new() {
    let mut writer = DownWriter::new(ATT_MTU);
    let first = alone(&mut writer, &HostToBridge::GetStatus);
    // The `0x00` is the first byte of the first piece.
    assert_eq!(first.len(), 1, "{first:?}");
    assert_eq!(first[0].kind, WriteKind::Request);
    assert_eq!(bytes(&first), led(&HostToBridge::GetStatus));
    let again = alone(&mut writer, &HostToBridge::GetStatus);
    assert_eq!(bytes(&again), host_frame(&HostToBridge::GetStatus), "only the first");
}

#[test]
fn ble_writer_leads_again_when_a_new_connection_starts() {
    let mut writer = DownWriter::new(ATT_MTU);
    alone(&mut writer, &HostToBridge::GetStatus);
    let mut reconnected = DownWriter::new(ATT_MTU);
    let first = alone(&mut reconnected, &HostToBridge::GetStatus);
    assert_eq!(bytes(&first), led(&HostToBridge::GetStatus));
}

#[test]
fn ble_writer_keeps_lead_when_nothing_was_offered() {
    let mut writer = DownWriter::new(ATT_MTU);
    assert!(writer.take().is_empty());
    assert_eq!(bytes(&alone(&mut writer, &HostToBridge::GetStatus)), led(&HostToBridge::GetStatus));
}

#[test]
fn ble_writer_caps_piece_when_mtu_allows_more() {
    // A write of more than 179 value bytes can reach the C6's host cut short (`wartui_proto::ble`).
    for mtu in [247, 517] {
        let mut writer = DownWriter::new(mtu);
        let writes = alone(&mut writer, &to_node(1000));
        assert!(writes.len() > 1);
        assert!(writes.iter().all(|w| w.bytes.len() <= DOWN_VALUE_MAX), "MTU {mtu}");
        assert!(writes[..writes.len() - 1].iter().all(|w| w.bytes.len() == DOWN_VALUE_MAX));
    }
}

#[test]
fn ble_writer_splits_frame_into_mtu_less_header_when_frame_is_long() {
    let cmd = to_node(1000);
    let mut writer = DownWriter::new(100);
    alone(&mut writer, &HostToBridge::GetStatus);
    let writes = alone(&mut writer, &cmd);
    assert!(writes.len() > 1);
    assert!(writes.iter().all(|w| w.bytes.len() <= 97), "ATT MTU 100 less its 3-byte header");
    assert!(writes[..writes.len() - 1].iter().all(|w| w.bytes.len() == 97));
    // Reassembled as the P4 does, the writes are one frame.
    let mut acc = FrameAccumulator::<MAX_HOST_FRAME>::new();
    let mut decoded = None;
    for byte in bytes(&writes) {
        if let Some(frame) = acc.push(byte) {
            decoded = Some(decode_host_frame(frame));
        }
    }
    assert_eq!(decoded, Some(Ok(Some(cmd))));
}

#[test]
fn ble_writer_writes_twenty_bytes_when_mtu_is_the_default() {
    let cmd = to_node(64);
    let writes = alone(&mut DownWriter::new(23), &cmd);
    assert!(writes.iter().all(|w| w.bytes.len() <= 20));
    assert!(alone(&mut DownWriter::new(0), &cmd).iter().all(|w| w.bytes.len() <= 20));
}

#[test]
fn ble_writer_ends_frame_with_one_request_when_frame_takes_several_pieces() {
    let mut writer = DownWriter::new(ATT_MTU);
    alone(&mut writer, &HostToBridge::GetStatus);
    let writes = alone(&mut writer, &to_node(1000));
    assert!(writes.len() > 1);
    assert_eq!(requests(&writes), 1);
    assert_eq!(writes.last().unwrap().kind, WriteKind::Request);
}

#[test]
fn ble_writer_sends_request_when_frame_takes_one_piece() {
    let mut writer = DownWriter::new(ATT_MTU);
    alone(&mut writer, &HostToBridge::GetStatus);
    let writes = alone(&mut writer, &HostToBridge::GetStatus);
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].kind, WriteKind::Request);
}

#[test]
fn ble_writer_fits_the_c6_write_queue_when_frame_is_largest() {
    // TrouBLE 0.8 queues 8 ATT PDUs per connection on the C6 and drops the rest; a group stays a
    // write short of that, the first group's lead included.
    let largest = to_node(wartui_proto::link::MAX_NODE_DOWN);
    let writes = alone(&mut DownWriter::new(ATT_MTU), &largest);
    assert!(writes.len() < 8, "{} writes", writes.len());
    assert_eq!(frames(&writes), [largest]);
}

#[test]
fn ble_writer_packs_frames_into_one_group_when_they_fit() {
    let mut writer = DownWriter::new(ATT_MTU);
    alone(&mut writer, &HostToBridge::GetStatus);
    let sent = [HostToBridge::GetStatus, to_node(300), HostToBridge::StopScan, to_node(200)];
    for cmd in &sent {
        assert!(writer.offer(cmd).unwrap());
    }
    let group = writer.take();
    assert_eq!(frames(&group), sent);
    // The run is cut at the piece, not at each frame.
    let total: usize = sent.iter().map(|cmd| host_frame(cmd).len()).sum();
    assert_eq!(group.len(), total.div_ceil(DOWN_VALUE_MAX));
}

#[test]
fn ble_writer_ends_group_with_one_request_when_it_holds_several_frames() {
    let mut writer = DownWriter::new(ATT_MTU);
    for cmd in [HostToBridge::GetStatus, to_node(500), HostToBridge::GetStatus] {
        assert!(writer.offer(&cmd).unwrap());
    }
    let group = writer.take();
    assert_eq!(requests(&group), 1);
    assert_eq!(group.last().unwrap().kind, WriteKind::Request);
    assert_eq!(group[0].bytes[0], 0x00, "the lead");
    assert_eq!(group[0].kind, WriteKind::Command);
}

#[test]
fn ble_writer_refuses_frame_when_group_is_full() {
    let mut writer = DownWriter::new(ATT_MTU);
    alone(&mut writer, &HostToBridge::GetStatus);
    let chunk = to_node(1000);
    assert!(writer.offer(&chunk).unwrap());
    // Two such frames take more pieces than a group holds.
    assert!(!writer.offer(&chunk).unwrap());
    assert_eq!(
        frames(&writer.take()),
        std::slice::from_ref(&chunk),
        "the refused frame is not in it"
    );
    // It waits for the next group, which takes it.
    assert_eq!(frames(&alone(&mut writer, &chunk)), [chunk]);
}

#[test]
fn ble_writer_takes_oversize_frame_alone_when_group_is_empty() {
    // At the default MTU of 23 one frame takes far more pieces than the C6 queues.
    let mut writer = DownWriter::new(23);
    alone(&mut writer, &HostToBridge::GetStatus);
    let largest = to_node(wartui_proto::link::MAX_NODE_DOWN);
    assert!(writer.offer(&largest).unwrap());
    assert!(!writer.offer(&HostToBridge::GetStatus).unwrap(), "nothing joins it");
    let group = writer.take();
    assert!(group.len() > 8, "{} writes", group.len());
    assert_eq!(frames(&group), [largest]);
}

#[test]
fn ble_writer_fits_the_c6_write_queue_when_group_is_full() {
    // TrouBLE 0.8 queues 8 ATT PDUs per connection on the C6 and drops the rest; a group stays
    // a write short of that, its lead included.
    for len in [1, 100, 500, 1000, wartui_proto::link::MAX_NODE_DOWN] {
        let mut writer = DownWriter::new(ATT_MTU);
        let mut offered = 0;
        while writer.offer(&to_node(len)).unwrap() {
            offered += 1;
        }
        let group = writer.take();
        assert!(group.len() < 8, "{} writes of {offered} frames of {len}", group.len());
        assert_eq!(frames(&group).len(), offered);
    }
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
fn ble_inbound_reports_incompatible_when_ready_is_from_another_major() {
    // Its frozen core proves the bridge, and its other messages are not read.
    let other = Version::new(BRIDGE_VERSION.major + 1, 0, 0);
    let mut bytes = frame(&ready_on(other, BRIDGE, 500));
    bytes.extend(frame(&rx(20)));
    bytes.extend(frame(&takeover()));
    let mut inbound = Inbound::new(None);
    let heard = inbound.feed(&bytes, Instant::now());
    assert_eq!(heard[0], Heard::Proved(BRIDGE), "remembered as a readable bridge is");
    let events = events(heard);
    let [LinkEvent::Incompatible { core, fresh: true }] = &events[..] else {
        panic!("not reported incompatible: {events:?}")
    };
    assert_eq!((core.mac, core.version), (BRIDGE, other));
    assert_eq!(core.standing, Standing::Apart { older: End::Host });
    assert!(inbound.announced(), "it answered, so the stream has started");
}

#[test]
fn ble_inbound_reports_wrong_bridge_when_unread_ready_names_another_address() {
    // The address is in the frozen core, so the spec is checked as for a readable `Ready`.
    let other = Version::new(BRIDGE_VERSION.major + 1, 0, 0);
    let elsewhere = [0x02, 0xF2, 0x62, 0x10, 0x44, 0xC0];
    let mut inbound = Inbound::new(Some(spec("02:00:5E:10:44:C0")));
    let heard = inbound.feed(&frame(&ready_on(other, elsewhere, 500)), Instant::now());
    assert_eq!(heard, [Heard::WrongBridge(elsewhere)]);
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
    let cause = DisconnectCause::UsbTookBridge;
    assert_eq!(gone, [LinkEvent::Disconnected { reason: USB_TOOK_BRIDGE.to_owned(), cause }]);
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
fn ble_inbound_reports_again_when_same_life_answers_twice() {
    // An update polls `Identify` and reads each answer.
    let mut inbound = Inbound::new(None);
    let t0 = Instant::now();
    inbound.feed(&frame(&ready(BRIDGE, 10_000)), t0);
    let heard = inbound.feed(&frame(&ready(BRIDGE, 11_000)), t0 + Duration::from_secs(1));
    assert!(!heard.contains(&Heard::Proved(BRIDGE)), "proved once per connection");
    let again = events(heard);
    assert!(matches!(&again[..], [LinkEvent::Again(core)] if core.uptime_ms == 11_000));
}

#[test]
fn ble_inbound_refuses_bridge_when_it_shares_only_the_advertised_octets() {
    let other = [0x02, 0xF2, 0x62, 0x10, 0x44, 0xC0];
    let mut inbound = Inbound::new(Some(spec("02:00:5E:10:44:C0")));
    let heard = inbound.feed(&frame(&ready(other, 500)), Instant::now());
    assert_eq!(heard, [Heard::WrongBridge(other)]);
}

#[test]
fn ble_settled_spec_names_settled_bridge_when_run_has_settled() {
    // The settled bridge already matched the operator's spec, and names it whole.
    assert_eq!(settled_spec(Some(&spec("44:C0")), Some(BRIDGE)), Some(BridgeSpec::Mac(BRIDGE)));
    assert_eq!(settled_spec(None, Some(BRIDGE)), Some(BridgeSpec::Mac(BRIDGE)));
}

#[test]
fn ble_settled_spec_passes_spec_through_when_nothing_has_settled() {
    assert_eq!(settled_spec(Some(&spec("44:C0")), None), Some(spec("44:C0")));
    assert_eq!(settled_spec(None, None), None);
}

#[test]
fn ble_inbound_refuses_other_bridge_when_run_settled_on_one_sharing_its_octets() {
    // The run was started with no spec, and settled on `BRIDGE`.
    let other = [0x02, 0xF2, 0x62, 0x10, 0x44, 0xC0];
    let mut inbound = Inbound::new(settled_spec(None, Some(BRIDGE)));
    let heard = inbound.feed(&frame(&ready(other, 500)), Instant::now());
    assert_eq!(heard, [Heard::WrongBridge(other)]);
}

#[test]
fn ble_inbound_admits_settled_bridge_when_it_reconnects() {
    let mut inbound = Inbound::new(settled_spec(Some(&spec("44:C0")), Some(BRIDGE)));
    let heard = inbound.feed(&frame(&ready(BRIDGE, 500)), Instant::now());
    assert_eq!(heard[0], Heard::Proved(BRIDGE));
    assert!(matches!(events(heard)[..], [LinkEvent::Connected(_)]));
}

#[test]
fn ble_drop_reads_as_maybe_not_bonded_when_bridge_hangs_up_about_ten_seconds_in_with_no_ready() {
    let after = |secs| Some(Duration::from_secs(secs));
    assert!(dropped_as_unbonded(after(10), false));
    assert!(dropped_as_unbonded(after(30), false), "inside the pairing window");
    assert!(!dropped_as_unbonded(after(10), true), "a bridge that announced");
    assert!(!dropped_as_unbonded(after(1), false), "a connect that failed");
    assert!(!dropped_as_unbonded(after(120), false));
}

#[test]
fn ble_drop_proves_nothing_when_link_was_up_before_this_connect() {
    // BlueZ held the link already, so nothing says when the C6's deadline started.
    assert!(!dropped_as_unbonded(None, false));
}

#[test]
fn ble_error_words_weak_case_apart_when_bridge_only_dropped_this_pc() {
    let refused = TransportError::NotBonded { bridge: "44:C0".to_owned() }.to_string();
    assert!(refused.contains("refused this PC over Bluetooth because they are not paired"));
    let dropped = TransportError::MaybeNotBonded { bridge: "44:C0".to_owned() }.to_string();
    assert!(dropped.contains("dropped this PC over Bluetooth before serving it"), "{dropped}");
    // The fix leads, so a fault box that cuts the reason short keeps it.
    assert!(dropped.starts_with("if this PC is not paired"), "{dropped}");
}

const OTHER: [u8; 6] = [0x02, 0x00, 0x5E, 0x10, 0xA1, 0xB0];
const ADDRESS: &str = "02:00:5E:10:44:C1";

#[test]
fn ble_look_connects_to_remembered_address_when_no_direct_connect_has_failed() {
    let looked = look(None, Some(BRIDGE), Some(ADDRESS.to_owned()), false);
    let want = Look { direct: Some(ADDRESS.to_owned()), preferred: Some(BridgeSpec::Mac(BRIDGE)) };
    assert_eq!(looked, want);
    let named = look(Some(&spec("44:C0")), Some(BRIDGE), Some(ADDRESS.to_owned()), false);
    assert_eq!(named.direct.as_deref(), Some(ADDRESS), "the remembered bridge is the one named");
}

#[test]
fn ble_look_scans_preferring_remembered_bridge_when_direct_connect_failed() {
    // As the serial sweep moves on from the remembered CH343: another bridge advertising is
    // taken only while the remembered one is not heard.
    let looked = look(None, Some(BRIDGE), Some(ADDRESS.to_owned()), true);
    assert_eq!(looked, Look { direct: None, preferred: Some(BridgeSpec::Mac(BRIDGE)) });
}

#[test]
fn ble_look_scans_for_named_bridge_only_when_it_is_not_the_remembered_one() {
    let named = spec("A1:B0");
    let looked = look(Some(&named), Some(BRIDGE), Some(ADDRESS.to_owned()), false);
    assert_eq!(looked, Look { direct: None, preferred: Some(named) });
    assert_eq!(look(None, None, None, false), Look { direct: None, preferred: None });
}

fn heard(octets: [u8; 2], live: bool) -> ([u8; 2], bool, [u8; 2]) {
    (octets, live, octets)
}

#[test]
fn ble_choose_takes_advertising_bridge_when_another_is_only_listed() {
    // BlueZ lists a bonded bridge it has not heard.
    let found = choose(vec![heard([0x44, 0xC0], false), heard([0xA1, 0xB0], true)], None);
    assert_eq!(found.unwrap(), [0xA1, 0xB0]);
}

#[test]
fn ble_choose_takes_preferred_bridge_when_several_advertise() {
    let both = vec![heard([0xA1, 0xB0], true), heard([0x44, 0xC0], true)];
    let found = choose(both.clone(), Some(&BridgeSpec::Mac(OTHER)));
    assert_eq!(found.unwrap(), [0xA1, 0xB0]);
    let refused = choose(both, None).unwrap_err().to_string();
    assert_eq!(
        refused,
        "several bridges advertise over Bluetooth (44:C0, A1:B0); name one with --bridge XX:YY"
    );
}

#[test]
fn ble_error_names_bridge_by_octets_when_none_advertised() {
    let named = TransportError::NotFound { bridge: Some("44:C0".to_owned()), secs: 12 };
    assert_eq!(
        named.to_string(),
        "bridge 44:C0 did not advertise over Bluetooth in 12s; check the bridge is powered and \
         in range"
    );
    let any = TransportError::NotFound { bridge: None, secs: 12 }.to_string();
    assert!(any.starts_with("no bridge advertised over Bluetooth in 12s"), "{any}");
}
