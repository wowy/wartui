//! The WardriveESP protocol the bridge speaks to a phone app: its frames, commands, lines and
//! session.

use wartui_proto::ChannelPool;
use wartui_proto::bus::{RecordKind, Security, SightingMsg};
use wartui_proto::plan::{SCAN_CHANNELS, index_of};
use wartui_proto::radio_link::{LastDrop, PhoneLink};
use wartui_proto::reset::ResetCause;
use wartui_proto::wardrive_esp::command::{
    self, COMMAND_MAX, ChannelList, Channels, Command, NICKNAME_MAX, parse_channels,
};
use wartui_proto::wardrive_esp::frame::{
    self, CTRL_UUID, HEADER_LEN, Inbound, RX_UUID, SERVICE_UUID, TX_UUID, TYPE_ACK, TYPE_CMD,
    TYPE_DATA, Taken, Write,
};
use wartui_proto::wardrive_esp::line::{self, LINE_MAX, Line};
use wartui_proto::wardrive_esp::session::{Ask, Boot, Class, Ctx, Session, Sink};

/// A framed write: the header with `version` and `flags`, then `payload`.
fn framed(kind: u8, id: u32, payload: &[u8], version: u8, flags: u8) -> Vec<u8> {
    let mut write = vec![0x47, 0x42, version, kind, 0, flags];
    write.extend_from_slice(&id.to_le_bytes());
    write.extend_from_slice(&u16::try_from(payload.len()).expect("fits").to_le_bytes());
    write.extend_from_slice(payload);
    write
}

fn cmd(id: u32, payload: &[u8]) -> Vec<u8> {
    framed(TYPE_CMD, id, payload, 1, 0x01)
}

#[test]
fn wardrive_uuids_stay_pinned_when_built() {
    // The phone apps look for these; a change strands every one of them.
    assert_eq!(SERVICE_UUID, 0x4768_6F73_7445_5350_4272_6964_6765_5376);
    assert_eq!(RX_UUID, 0x0147_686F_7374_4553_5042_7269_6467_5852);
    assert_eq!(TX_UUID, 0x0247_686F_7374_4553_5042_7269_6467_5854);
    assert_eq!(CTRL_UUID, 0x0347_686F_7374_4553_5042_7269_4C52_5443);
    assert_eq!(HEADER_LEN, 12);
    assert_eq!((TYPE_CMD, TYPE_ACK, TYPE_DATA), (1, 2, 3));
}

#[test]
fn wardrive_frame_reads_framed_write_when_magic_and_length_fit() {
    let write = cmd(0x0102_0304, b"caps");
    assert_eq!(
        frame::parse_write(&write),
        Write::Framed { kind: TYPE_CMD, id: 0x0102_0304, payload: b"caps" }
    );
    let empty = cmd(7, b"");
    assert_eq!(frame::parse_write(&empty), Write::Framed { kind: TYPE_CMD, id: 7, payload: b"" });
}

#[test]
fn wardrive_frame_reads_text_when_length_overruns_write() {
    let mut write = cmd(1, b"caps");
    write.pop();
    assert_eq!(frame::parse_write(&write), Write::Text(&write));
    // A length field of 0xFFFF in a short write, as a phone in range may send.
    let mut huge = cmd(1, b"caps");
    huge[10..12].copy_from_slice(&[0xFF, 0xFF]);
    assert_eq!(frame::parse_write(&huge), Write::Text(&huge));
}

#[test]
fn wardrive_frame_reads_text_when_write_is_shorter_than_header() {
    let write = [0x47, 0x42, 1, 1, 0, 0, 0, 0, 0, 0, 0];
    assert_eq!(frame::parse_write(&write), Write::Text(&write));
    assert_eq!(frame::parse_write(b"caps\n"), Write::Text(b"caps\n"));
    assert_eq!(frame::parse_write(b""), Write::Text(b""));
}

#[test]
fn wardrive_frame_ignores_version_and_flags_when_parsing() {
    let write = framed(TYPE_CMD, 9, b"stop", 0x7E, 0xFF);
    assert_eq!(
        frame::parse_write(&write),
        Write::Framed { kind: TYPE_CMD, id: 9, payload: b"stop" }
    );
}

#[test]
fn wardrive_frame_ignores_bytes_past_payload_when_parsing() {
    let mut write = cmd(3, b"ch");
    write.extend_from_slice(b" all");
    assert_eq!(frame::parse_write(&write), Write::Framed { kind: TYPE_CMD, id: 3, payload: b"ch" });
}

#[test]
fn wardrive_frame_builds_ack_golden_when_given_id() {
    assert_eq!(frame::ack(0x0403_0201), [0x47, 0x42, 1, 2, 0, 0, 1, 2, 3, 4, 0, 0]);
}

#[test]
fn wardrive_frame_builds_data_header_golden_when_given_id_and_length() {
    assert_eq!(frame::data_header(0x0403_0201, 0x0605), [0x47, 0x42, 1, 3, 0, 0, 1, 2, 3, 4, 5, 6]);
}

#[test]
fn wardrive_frame_gives_room_when_mtu_is_182_and_23() {
    assert_eq!(frame::data_room(182), 167);
    assert_eq!(frame::data_room(23), 8);
    // Below the ATT minimum reads as the minimum.
    assert_eq!(frame::data_room(0), 8);
}

#[test]
fn wardrive_inbound_acks_cmd_with_its_id_when_framed() {
    let mut inbound = Inbound::new();
    let write = cmd(0x55, b"caps");
    assert_eq!(
        inbound.on_write(&write),
        Taken { ack: Some(frame::ack(0x55)), command: Some(b"caps") }
    );
    assert_eq!(inbound.reply_id(), 0x55);
}

#[test]
fn wardrive_inbound_updates_reply_id_and_runs_nothing_when_type_is_not_cmd() {
    let mut inbound = Inbound::new();
    for kind in [TYPE_ACK, TYPE_DATA, 4, 5, 0xEE] {
        let write = framed(kind, u32::from(kind) + 100, b"caps", 1, 0);
        assert_eq!(inbound.on_write(&write), Taken { ack: None, command: None });
        assert_eq!(inbound.reply_id(), u32::from(kind) + 100);
    }
}

#[test]
fn wardrive_inbound_keeps_reply_id_when_write_is_text() {
    let mut inbound = Inbound::new();
    let _ = inbound.on_write(&cmd(42, b"start"));
    assert_eq!(inbound.on_write(b"caps\n"), Taken { ack: None, command: Some(b"caps\n") });
    assert_eq!(inbound.reply_id(), 42);
}

#[test]
fn wardrive_inbound_starts_at_reply_id_one_when_new() {
    assert_eq!(Inbound::new().reply_id(), 1);
    assert_eq!(Inbound::default().reply_id(), 1);
}

fn channels(text: &[u8], pool: ChannelPool) -> Option<Vec<u8>> {
    parse_channels(text, pool).map(|list| list.channels().to_vec())
}

#[test]
fn wardrive_command_trims_and_truncates_when_parsing() {
    assert_eq!(command::trim(b"  caps \r\n"), b"caps");
    // An inner newline is kept: one write is one command.
    assert_eq!(command::trim(b"caps\nstop\n"), b"caps\nstop");
    let mut long = b"ch ".to_vec();
    long.resize(COMMAND_MAX + 10, b'1');
    assert_eq!(command::trim(&long).len(), COMMAND_MAX);
    // Trailing spaces past the cut are not what is trimmed.
    let mut padded = vec![b'x'; COMMAND_MAX - 1];
    padded.extend_from_slice(b" y");
    assert_eq!(command::trim(&padded), &padded[..COMMAND_MAX - 1]);
    assert_eq!(command::parse(b"  status\r\n"), Some(Command::Status));
}

#[test]
fn wardrive_command_reads_nothing_when_text_is_blank() {
    assert_eq!(command::parse(b""), None);
    assert_eq!(command::parse(b"   \r\n"), None);
    assert_eq!(command::parse(b"\n"), None);
}

#[test]
fn wardrive_command_reads_stop_when_wdstream_contains_stop() {
    assert_eq!(command::parse(b"wdstream stop"), Some(Command::Stop));
    assert_eq!(command::parse(b"wdstream start -wifi stop"), Some(Command::Stop));
    assert_eq!(command::parse(b"stop"), Some(Command::Stop));
    assert_eq!(command::parse(b"start"), Some(Command::Start));
}

#[test]
fn wardrive_command_reads_channels_when_wdstream_carries_ch() {
    assert_eq!(
        command::parse(b"wdstream start -wifi -ch 1,6,11,36"),
        Some(Command::Stream { channels: Some(b"1,6,11,36"), unsupported_mode: false })
    );
    assert_eq!(
        command::parse(b"wdstream start -ch 1,6 -wifi"),
        Some(Command::Stream { channels: Some(b"1,6 -wifi"), unsupported_mode: false })
    );
    assert_eq!(
        command::parse(b"wdstream start -wifi"),
        Some(Command::Stream { channels: None, unsupported_mode: false })
    );
    // `-ch` with no space after it names no list.
    assert_eq!(
        command::parse(b"wdstream start -wifi -ch"),
        Some(Command::Stream { channels: None, unsupported_mode: false })
    );
    assert_eq!(
        command::parse(b"wdstream start -ch ,,, -wifi"),
        Some(Command::Stream { channels: Some(b",,, -wifi"), unsupported_mode: false })
    );
}

#[test]
fn wardrive_command_flags_mode_when_wdstream_asks_for_bluetooth() {
    for text in [&b"wdstream start -bt"[..], b"wdstream start -cap", b"wdstream start -promisc"] {
        assert_eq!(
            command::parse(text),
            Some(Command::Stream { channels: None, unsupported_mode: true }),
            "{}",
            String::from_utf8_lossy(text)
        );
    }
}

#[test]
fn wardrive_command_reads_each_channel_form_when_word_is_ch() {
    assert_eq!(command::parse(b"ch"), Some(Command::Channels(Channels::Show)));
    assert_eq!(command::parse(b"ch all"), Some(Command::Channels(Channels::All)));
    assert_eq!(command::parse(b"ch 1,6,11"), Some(Command::Channels(Channels::Set(b"1,6,11"))));
    assert_eq!(command::parse(b"caps"), Some(Command::Caps));
    assert_eq!(command::parse(b"help"), Some(Command::Help));
    assert_eq!(command::parse(b"info"), Some(Command::Info));
    assert_eq!(command::parse(b"status"), Some(Command::Status));
    assert_eq!(command::parse(b"link"), Some(Command::Link));
    assert_eq!(command::parse(b"drop"), Some(Command::Drop));
}

#[test]
fn wardrive_command_is_silent_when_app_sends_bye_hb_client_time() {
    for text in [&b"bye"[..], b"hb", b"client android 2.1", b"time 1763380000"] {
        assert_eq!(
            command::parse(text),
            Some(Command::Silent),
            "{}",
            String::from_utf8_lossy(text)
        );
    }
}

#[test]
fn wardrive_command_is_unsupported_when_word_names_a_missing_mode() {
    for word in
        ["bt", "cap on", "csi start ch=6", "coproc", "led test", "screen", "sd", "ota begin 9"]
    {
        assert_eq!(command::parse(word.as_bytes()), Some(Command::Unsupported), "{word}");
    }
}

#[test]
fn wardrive_command_names_word_when_unknown() {
    // `reboot` is unknown on purpose: any phone in range may send it.
    assert_eq!(command::parse(b"reboot"), Some(Command::Unknown(b"reboot")));
    assert_eq!(command::parse(b"frobnicate x"), Some(Command::Unknown(b"frobnicate")));
    assert_eq!(command::parse(b"CAPS"), Some(Command::Unknown(b"CAPS")));
    assert_eq!(command::parse(b"links"), Some(Command::Unknown(b"links")));
    let word = [0xFFu8; COMMAND_MAX];
    assert_eq!(command::parse(&word), Some(Command::Unknown(&word[..])));
}

#[test]
fn wardrive_channels_keep_order_and_drop_duplicates_when_parsing() {
    assert_eq!(channels(b"11,1,6,11", ChannelPool::All), Some(vec![11, 1, 6]));
}

#[test]
fn wardrive_channels_stop_at_first_non_digit_when_parsing() {
    assert_eq!(channels(b"1,6 x,11", ChannelPool::All), Some(vec![1, 6]));
    assert_eq!(channels(b"1,,6,", ChannelPool::All), Some(vec![1, 6]));
}

#[test]
fn wardrive_channels_read_spaces_as_separators_when_parsing() {
    assert_eq!(channels(b"1, 6, 11", ChannelPool::All), Some(vec![1, 6, 11]));
    assert_eq!(channels(b"1 6 11", ChannelPool::All), Some(vec![1, 6, 11]));
}

#[test]
fn wardrive_channels_drop_channel_when_pool_lacks_it() {
    let in_us = |channel| index_of(channel).is_some_and(|idx| ChannelPool::Us.contains(idx));
    assert!(in_us(1) && in_us(165) && !in_us(14));
    assert_eq!(channels(b"1,14,165,999", ChannelPool::Us), Some(vec![1, 165]));
    // 255 past a byte's reach would wrap onto a real channel if it were not dropped.
    assert_eq!(channels(b"262,6", ChannelPool::All), Some(vec![6]));
}

#[test]
fn wardrive_channels_refuse_list_when_none_in_pool() {
    assert_eq!(channels(b"14,200", ChannelPool::All), None);
    assert_eq!(channels(b"", ChannelPool::All), None);
    assert_eq!(channels(b",,,", ChannelPool::All), None);
}

#[test]
fn wardrive_channels_list_whole_pool_when_built_from_all() {
    for pool in [ChannelPool::Us, ChannelPool::Eu, ChannelPool::All] {
        let list = ChannelList::all(pool);
        let want: Vec<u8> = SCAN_CHANNELS
            .iter()
            .enumerate()
            .filter(|(idx, _)| pool.contains(u8::try_from(*idx).expect("fits")))
            .map(|(_, ch)| *ch)
            .collect();
        assert_eq!(list.channels(), want);
        assert!(list.contains(36));
        assert!(!list.contains(14));
        assert!(!list.contains(0));
    }
}

const BSSID: [u8; 6] = [0x02, 0x00, 0x5E, 0x10, 0x44, 0xC0];

fn sighting(channel: u8, ssid: &[u8]) -> SightingMsg<'_> {
    SightingMsg {
        kind: RecordKind::Wifi,
        bssid: BSSID,
        channel,
        rssi: -47,
        security: Security::Wpa2Psk,
        ssid,
        // Never read: the trailer is wartui's, not the protocol's.
        ext: &[0xAA, 0xBB],
    }
}

fn text(out: &Line) -> &str {
    std::str::from_utf8(out).expect("UTF-8")
}

#[test]
fn wardrive_line_formats_ap_golden_when_ssid_is_set() {
    let mut out = Line::new();
    assert!(line::ap(&mut out, 81_234, &sighting(6, b"coffee")));
    assert_eq!(
        text(&out),
        "WD:AP ts=81234 bssid=02:00:5E:10:44:C0 ssid_hex=636f66666565 rssi=-47 ch=6 auth=WPA2 \
         hidden=0\n"
    );
}

#[test]
fn wardrive_line_marks_hidden_when_ssid_is_empty() {
    let mut out = Line::new();
    assert!(line::ap(&mut out, 5, &sighting(36, b"")));
    assert_eq!(
        text(&out),
        "WD:AP ts=5 bssid=02:00:5E:10:44:C0 ssid_hex= rssi=-47 ch=36 auth=WPA2 hidden=1\n"
    );
}

#[test]
fn wardrive_line_formats_ap_when_ssid_is_not_utf8_and_full_length() {
    let mut out = Line::new();
    let ssid = [0xFFu8; 32];
    assert!(line::ap(&mut out, u32::MAX, &sighting(177, &ssid)));
    let want = format!(
        "WD:AP ts=4294967295 bssid=02:00:5E:10:44:C0 ssid_hex={} rssi=-47 ch=177 auth=WPA2 \
         hidden=0\n",
        "ff".repeat(32)
    );
    assert_eq!(text(&out), want);
}

#[test]
fn wardrive_line_maps_security_to_vendor_token_when_formatting_auth() {
    let cases = [
        (Security::Open, "OPEN"),
        (Security::Wep, "WEP"),
        (Security::WpaPsk, "WPA"),
        (Security::Wpa2Psk, "WPA2"),
        (Security::WpaWpa2Psk, "WPA2"),
        (Security::Wpa2Enterprise, "WPA2"),
        (Security::Wpa3Psk, "WPA3"),
        (Security::Wpa2Wpa3Psk, "WPA2_WPA3"),
        (Security::WapiPsk, "UNKNOWN"),
        (Security::Undefined, "UNKNOWN"),
        (Security::Ble, "UNKNOWN"),
        (Security::Unknown(77), "UNKNOWN"),
    ];
    for (security, token) in cases {
        assert_eq!(line::auth_token(security), token, "{security:?}");
    }
}

#[test]
fn wardrive_line_formats_caps_golden_when_pool_is_us() {
    let mut out = Line::new();
    assert!(line::caps(&mut out, ChannelPool::Us));
    assert_eq!(
        text(&out),
        "WD:CAPS fw=1.6.0 chip=esp32c5 bands=2g,5g ota=0 bt=0 gps=0 cap=0 usb=0 board=wartui \
         board_name=wartui link=ble chans=1,2,3,4,5,6,7,8,9,10,11,36,40,44,48,52,56,60,64,100,\
         104,108,112,116,120,124,128,132,136,140,144,149,153,157,161,165\n"
    );
    assert_eq!(line::FW, "1.6.0");
}

#[test]
fn wardrive_line_fits_caps_when_pool_is_all() {
    let mut out = Line::new();
    assert!(line::caps(&mut out, ChannelPool::All));
    assert!(text(&out).ends_with(",169,173,177\n"));
    assert!(out.len() <= LINE_MAX);
}

#[test]
fn wardrive_line_formats_plan_status_end_err_golden_when_built() {
    let mut out = Line::new();
    let list = parse_channels(b"1,6,11", ChannelPool::All).expect("a list");
    assert!(line::plan(&mut out, &list));
    assert_eq!(text(&out), "WD:PLAN chans=1,6,11\n");
    assert!(line::status(&mut out, 12, 3));
    assert_eq!(text(&out), "WD:STATUS aps=12 scans=3\n");
    assert!(line::end(&mut out));
    assert_eq!(text(&out), "WD:END reason=stop\n");
    assert!(line::err(&mut out, "no_supported_channels"));
    assert_eq!(text(&out), "WD:ERR reason=no_supported_channels\n");
    assert!(line::mode(&mut out));
    assert_eq!(text(&out), "WD:MODE mode=wifi\n");
    assert!(line::begin(&mut out));
    assert_eq!(text(&out), "WD:BEGIN type=wifi interval=0 channel=auto\n");
    assert!(line::text(&mut out, format_args!("aps {} scans {}", 4, 5)));
    assert_eq!(text(&out), "aps 4 scans 5\n");
    // A line too long for `LINE_MAX` is dropped whole.
    let long = "x".repeat(LINE_MAX);
    assert!(!line::text(&mut out, format_args!("{long}")));
    assert!(out.is_empty());
}

/// Every line a session sent, with its class.
#[derive(Default)]
struct Lines(Vec<(Class, Vec<u8>)>);

impl Sink for Lines {
    fn line(&mut self, class: Class, line: &[u8]) {
        self.0.push((class, line.to_vec()));
    }
}

impl Lines {
    /// The lines sent since the last call, as text.
    fn take(&mut self) -> Vec<String> {
        self.take_classed().into_iter().map(|(_, line)| line).collect()
    }

    fn take_classed(&mut self) -> Vec<(Class, String)> {
        self.0
            .drain(..)
            .map(|(class, line)| (class, String::from_utf8(line).expect("UTF-8")))
            .collect()
    }
}

/// The `All` pool's channels, comma-separated.
const ALL_CHANS: &str = "1,2,3,4,5,6,7,8,9,10,11,12,13,36,40,44,48,52,56,60,64,100,104,108,112,\
                         116,120,124,128,132,136,140,144,149,153,157,161,165,169,173,177";

const HELP: [&str; 14] = [
    "wartui bridge, WardriveESP commands:\n",
    "  wdstream start -wifi [-ch 1,6,11]\n",
    "  wdstream stop\n",
    "  start\n",
    "  stop\n",
    "  caps\n",
    "  ch\n",
    "  ch 1,6,11\n",
    "  ch all\n",
    "  status\n",
    "  info\n",
    "  link\n",
    "  drop\n",
    "  help\n",
];

fn ctx() -> Ctx<'static> {
    Ctx {
        pool: ChannelPool::All,
        version: "0.9.0",
        bridge: [0x44, 0xC0],
        uptime_ms: 61_500,
        nodes: 3,
        mtu: 182,
        dropped: 2,
        link: PhoneLink {
            peer: [0x4A, 0x9F],
            connections: 3,
            connected_s: 65,
            rssi: Some(-61),
            interval_us: 48_750,
            timeout_ms: 4000,
            last_drop: Some(LastDrop { reason: 0x13, ago_s: 12 }),
            heap_free: 41_000,
            heap_least: 38_000,
        },
        boot: Boot { slot: "ota_0", trial: false, reset: ResetCause::PowerOn },
    }
}

const BOOT: &str = "WD:BOOT fw=1.6.0 slot=ota_0 image=confirmed reset=power_on\n";
const LINK: &str = "WD:LINK conns=3 mtu=182 prev_disc=0x213 prev_disc_ago=12s\n";

fn caps_all() -> String {
    format!(
        "WD:CAPS fw=1.6.0 chip=esp32c5 bands=2g,5g ota=0 bt=0 gps=0 cap=0 usb=0 board=wartui \
         board_name=wartui link=ble chans={ALL_CHANS}\n"
    )
}

fn header(plan: &str) -> Vec<String> {
    vec![
        caps_all(),
        BOOT.into(),
        LINK.into(),
        "WD:MODE mode=wifi\n".into(),
        "WD:BEGIN type=wifi interval=0 channel=auto\n".into(),
        format!("WD:PLAN chans={plan}\n"),
    ]
}

/// A session the phone has subscribed to, and the lines that took, drained.
fn subscribed() -> (Session, Lines) {
    let mut session = Session::new();
    let mut lines = Lines::default();
    assert_eq!(session.on_subscribe(&ctx(), &mut lines), Some(Ask::Start));
    lines.take();
    (session, lines)
}

fn ap_line(ts: u32, channel: u8) -> String {
    format!(
        "WD:AP ts={ts} bssid=02:00:5E:10:44:C0 ssid_hex=636f66666565 rssi=-47 ch={channel} \
         auth=WPA2 hidden=0\n"
    )
}

#[test]
fn wardrive_session_sends_header_in_order_when_phone_subscribes() {
    let mut session = Session::new();
    let mut lines = Lines::default();
    session.on_subscribe(&ctx(), &mut lines);
    let header: Vec<_> = header(ALL_CHANS).into_iter().map(|line| (Class::Stream, line)).collect();
    assert_eq!(lines.take_classed(), header);
}

#[test]
fn wardrive_session_asks_scan_start_when_phone_subscribes() {
    let mut session = Session::new();
    assert!(!session.scanning());
    assert_eq!(session.on_subscribe(&ctx(), &mut Lines::default()), Some(Ask::Start));
    assert!(session.scanning());
}

#[test]
fn wardrive_session_sends_ap_when_scanning_and_channel_is_listed() {
    let (mut session, mut lines) = subscribed();
    session.on_command(b"ch 1,6", &ctx(), &mut lines);
    lines.take();
    session.on_sighting(81_234, &sighting(6, b"coffee"), &mut lines);
    assert_eq!(lines.take_classed(), [(Class::Stream, ap_line(81_234, 6))]);
}

#[test]
fn wardrive_session_filters_ap_when_channel_is_off_the_list() {
    let (mut session, mut lines) = subscribed();
    session.on_command(b"ch 1,6", &ctx(), &mut lines);
    lines.take();
    for channel in [11, 0, 14, 200, 255] {
        session.on_sighting(1, &sighting(channel, b"coffee"), &mut lines);
    }
    assert!(lines.take().is_empty());
    session.on_period(&mut lines);
    assert_eq!(lines.take(), ["WD:STATUS aps=0 scans=1\n"]);
}

#[test]
fn wardrive_session_sends_ap_on_any_channel_when_list_is_all() {
    let (mut session, mut lines) = subscribed();
    session.on_command(b"ch 1,6", &ctx(), &mut lines);
    session.on_command(b"ch all", &ctx(), &mut lines);
    lines.take();
    for channel in [1, 11, 177] {
        session.on_sighting(7, &sighting(channel, b"coffee"), &mut lines);
    }
    assert_eq!(lines.take(), [ap_line(7, 1), ap_line(7, 11), ap_line(7, 177)]);
}

#[test]
fn wardrive_session_skips_record_when_kind_is_ble() {
    let (mut session, mut lines) = subscribed();
    let ble = SightingMsg {
        kind: RecordKind::Ble,
        channel: 0,
        security: Security::Ble,
        ssid: b"",
        ..sighting(0, b"")
    };
    session.on_sighting(1, &ble, &mut lines);
    assert!(lines.take().is_empty());
}

#[test]
fn wardrive_session_sends_no_ap_when_idle() {
    let mut session = Session::new();
    let mut lines = Lines::default();
    session.on_sighting(1, &sighting(6, b"coffee"), &mut lines);
    session.on_period(&mut lines);
    assert!(lines.take().is_empty());
}

#[test]
fn wardrive_session_counts_aps_since_last_status_when_period_ends() {
    let (mut session, mut lines) = subscribed();
    for _ in 0..3 {
        session.on_sighting(1, &sighting(6, b"coffee"), &mut lines);
    }
    session.on_period(&mut lines);
    session.on_sighting(2, &sighting(6, b"coffee"), &mut lines);
    session.on_period(&mut lines);
    let lines = lines.take();
    assert_eq!(lines[3], "WD:STATUS aps=3 scans=1\n");
    assert_eq!(lines[5], "WD:STATUS aps=1 scans=2\n");
}

#[test]
fn wardrive_session_sends_status_then_end_when_stopped() {
    let (mut session, mut lines) = subscribed();
    session.on_sighting(1, &sighting(6, b"coffee"), &mut lines);
    session.on_sighting(1, &sighting(6, b"coffee"), &mut lines);
    session.on_period(&mut lines);
    session.on_sighting(2, &sighting(6, b"coffee"), &mut lines);
    lines.take();
    assert_eq!(session.on_command(b"wdstream stop", &ctx(), &mut lines), Some(Ask::Stop));
    assert_eq!(
        lines.take_classed(),
        [
            (Class::Stream, "WD:STATUS aps=1 scans=1\n".into()),
            (Class::Stream, "WD:END reason=stop\n".into()),
        ]
    );
    assert!(!session.scanning());
}

#[test]
fn wardrive_session_says_nothing_when_stopped_while_idle() {
    let mut session = Session::new();
    let mut lines = Lines::default();
    assert_eq!(session.on_command(b"stop", &ctx(), &mut lines), None);
    assert_eq!(session.on_command(b"wdstream stop", &ctx(), &mut lines), None);
    assert!(lines.take().is_empty());
}

#[test]
fn wardrive_session_sends_header_again_when_started_after_stop() {
    let (mut session, mut lines) = subscribed();
    session.on_command(b"stop", &ctx(), &mut lines);
    lines.take();
    assert_eq!(session.on_command(b"start", &ctx(), &mut lines), Some(Ask::Start));
    assert_eq!(lines.take(), header(ALL_CHANS));
    // Already scanning: nothing to start, nothing said.
    assert_eq!(session.on_command(b"start", &ctx(), &mut lines), None);
    assert!(lines.take().is_empty());
}

#[test]
fn wardrive_session_counts_from_zero_when_started_after_stop() {
    let (mut session, mut lines) = subscribed();
    session.on_sighting(1, &sighting(6, b"coffee"), &mut lines);
    session.on_sighting(1, &sighting(6, b"coffee"), &mut lines);
    session.on_period(&mut lines);
    session.on_sighting(2, &sighting(6, b"coffee"), &mut lines);
    session.on_command(b"stop", &ctx(), &mut lines);
    session.on_command(b"start", &ctx(), &mut lines);
    lines.take();
    session.on_command(b"status", &ctx(), &mut lines);
    assert_eq!(lines.take()[1], "aps 0 scans 0\n");
    session.on_sighting(3, &sighting(6, b"coffee"), &mut lines);
    session.on_period(&mut lines);
    assert_eq!(lines.take().last().map(String::as_str), Some("WD:STATUS aps=1 scans=1\n"));
}

#[test]
fn wardrive_session_keeps_list_when_stopped_and_started() {
    let (mut session, mut lines) = subscribed();
    session.on_command(b"ch 1,6", &ctx(), &mut lines);
    session.on_command(b"stop", &ctx(), &mut lines);
    lines.take();
    session.on_command(b"wdstream start -wifi", &ctx(), &mut lines);
    assert_eq!(lines.take(), header("1,6"));
    session.on_sighting(1, &sighting(11, b"coffee"), &mut lines);
    assert!(lines.take().is_empty());
}

#[test]
fn wardrive_session_answers_plan_when_wdstream_sets_channels_while_scanning() {
    let (mut session, mut lines) = subscribed();
    let scan = session.on_command(b"wdstream start -wifi -ch 1,6,11,36", &ctx(), &mut lines);
    assert_eq!(scan, None);
    assert_eq!(
        lines.take_classed(),
        [
            (Class::Answer, "WD:PLAN chans=1,6,11,36\n".into()),
            (Class::Answer, "WD:MODE mode=wifi\n".into()),
        ]
    );
}

#[test]
fn wardrive_session_keeps_list_and_errs_when_no_channel_is_supported() {
    let (mut session, mut lines) = subscribed();
    session.on_command(b"ch 1,6", &ctx(), &mut lines);
    lines.take();
    for command in [&b"ch 14,200"[..], b"wdstream start -ch ,, -wifi"] {
        session.on_command(command, &ctx(), &mut lines);
        let lines = lines.take_classed();
        assert_eq!(lines[0], (Class::Answer, "WD:ERR reason=no_supported_channels\n".into()));
    }
    session.on_command(b"ch", &ctx(), &mut lines);
    assert_eq!(lines.take(), [format!("supported {ALL_CHANS}\n"), "scanning 1,6\n".into()]);
}

#[test]
fn wardrive_session_keeps_list_when_ch_flag_ends_command() {
    let (mut session, mut lines) = subscribed();
    session.on_command(b"ch 1,6", &ctx(), &mut lines);
    lines.take();
    session.on_command(b"wdstream start -wifi -ch", &ctx(), &mut lines);
    assert!(lines.take().iter().all(|line| !line.starts_with("WD:ERR")));
    session.on_command(b"ch", &ctx(), &mut lines);
    assert_eq!(lines.take(), [format!("supported {ALL_CHANS}\n"), "scanning 1,6\n".into()]);
}

#[test]
fn wardrive_session_reads_list_to_flag_when_ch_precedes_wifi() {
    let (mut session, mut lines) = subscribed();
    session.on_command(b"wdstream start -ch 1, 6 -wifi", &ctx(), &mut lines);
    assert_eq!(lines.take()[0], "WD:PLAN chans=1,6\n");
}

#[test]
fn wardrive_session_starts_anyway_when_channel_list_errs() {
    let mut session = Session::new();
    let mut lines = Lines::default();
    let scan = session.on_command(b"wdstream start -wifi -ch 14", &ctx(), &mut lines);
    assert_eq!(scan, Some(Ask::Start));
    let mut want = vec!["WD:ERR reason=no_supported_channels\n".to_owned()];
    want.extend(header(ALL_CHANS));
    assert_eq!(lines.take(), want);
}

#[test]
fn wardrive_session_answers_unsupported_when_mode_is_missing() {
    let mut session = Session::new();
    let mut lines = Lines::default();
    for command in [&b"wdstream start -bt"[..], b"bt on", b"cap", b"ota begin 1000"] {
        assert_eq!(session.on_command(command, &ctx(), &mut lines), None);
        assert_eq!(lines.take_classed(), [(Class::Answer, "WD:ERR reason=unsupported\n".into())]);
    }
    assert!(!session.scanning());
}

#[test]
fn wardrive_session_answers_nothing_when_command_is_silent() {
    let (mut session, mut lines) = subscribed();
    for command in [&b"bye"[..], b"iam a pixel", b"hb", b"client x", b"time 9", b"", b"  \r\n"] {
        assert_eq!(session.on_command(command, &ctx(), &mut lines), None);
    }
    assert!(lines.take().is_empty());
    assert!(session.scanning());
}

#[test]
fn wardrive_session_names_word_and_lists_help_when_command_is_unknown() {
    let mut session = Session::new();
    let mut lines = Lines::default();
    session.on_command(b"reboot now", &ctx(), &mut lines);
    let mut want = vec!["unknown command 'reboot'\n".to_owned()];
    want.extend(HELP.map(String::from));
    assert_eq!(lines.take(), want);
    // A word that is not UTF-8 comes back as the bytes it was.
    session.on_command(&[0xC3, 0x28, b' ', b'x'], &ctx(), &mut lines);
    assert_eq!(lines.0[0].1, b"unknown command '\xC3\x28'\n");
    assert_eq!(lines.0.len(), 1 + HELP.len());
    lines.0.clear();
    session.on_command(b"help", &ctx(), &mut lines);
    assert_eq!(lines.take(), HELP);
}

#[test]
fn wardrive_session_survives_unknown_word_when_it_fills_a_command() {
    let mut session = Session::new();
    let mut lines = Lines::default();
    let mut word = vec![b'x'; COMMAND_MAX + 50];
    word.push(b'\n');
    session.on_command(&word, &ctx(), &mut lines);
    let lines = lines.take();
    assert_eq!(lines[0], format!("unknown command '{}'\n", "x".repeat(COMMAND_MAX)));
    assert_eq!(lines[1..], HELP);
}

#[test]
fn wardrive_session_resets_list_when_released() {
    let (mut session, mut lines) = subscribed();
    session.on_command(b"ch 1,6", &ctx(), &mut lines);
    session.release();
    lines.take();
    session.on_command(b"start", &ctx(), &mut lines);
    assert_eq!(lines.take(), header(ALL_CHANS));
}

#[test]
fn wardrive_session_asks_scan_stop_when_released_while_scanning() {
    let (mut session, _) = subscribed();
    assert_eq!(session.release(), Some(Ask::Stop));
    assert!(!session.scanning());
    assert_eq!(session.release(), None);
    assert_eq!(Session::new().release(), None);
}

#[test]
fn wardrive_session_resets_list_when_phone_subscribes_again() {
    let (mut session, mut lines) = subscribed();
    session.on_command(b"ch 1,6", &ctx(), &mut lines);
    session.on_sighting(1, &sighting(6, b"coffee"), &mut lines);
    session.on_period(&mut lines);
    lines.take();
    assert_eq!(session.on_subscribe(&ctx(), &mut lines), Some(Ask::Start));
    assert_eq!(lines.take(), header(ALL_CHANS));
    session.on_command(b"status", &ctx(), &mut lines);
    assert_eq!(lines.take()[1], "aps 0 scans 0\n");
}

#[test]
fn wardrive_session_classes_answers_apart_from_stream_when_scanning() {
    let (mut session, mut lines) = subscribed();
    session.on_sighting(1, &sighting(6, b"coffee"), &mut lines);
    session.on_period(&mut lines);
    assert!(lines.take_classed().iter().all(|(class, _)| *class == Class::Stream));
    let commands = [&b"caps"[..], b"status", b"info", b"link", b"drop", b"help", b"ch", b"ch 6"];
    for command in commands.into_iter().chain([&b"ch all"[..], b"x"]) {
        session.on_command(command, &ctx(), &mut lines);
        let classed = lines.take_classed();
        assert!(!classed.is_empty());
        assert!(classed.iter().all(|(class, _)| *class == Class::Answer));
    }
    session.on_command(b"stop", &ctx(), &mut lines);
    assert!(lines.take_classed().iter().all(|(class, _)| *class == Class::Stream));
}

#[test]
fn wardrive_session_answers_status_and_info_when_asked() {
    let (mut session, mut lines) = subscribed();
    session.on_command(b"ch 6,1", &ctx(), &mut lines);
    session.on_sighting(1, &sighting(6, b"coffee"), &mut lines);
    session.on_sighting(1, &sighting(6, b"coffee"), &mut lines);
    session.on_period(&mut lines);
    session.on_sighting(1, &sighting(6, b"coffee"), &mut lines);
    lines.take();
    session.on_command(b"status", &ctx(), &mut lines);
    assert_eq!(
        lines.take(),
        ["scanning on\n", "aps 2 scans 1\n", "channels 6,1\n", "mtu 182 dropped 2\n"]
    );
    session.on_command(b"info", &ctx(), &mut lines);
    assert_eq!(
        lines.take(),
        [
            "wartui bridge 44:C0 fw 0.9.0\n",
            "WardriveESP protocol 1.6.0\n",
            "uptime 61 s nodes 3\n",
            BOOT
        ]
    );
    session.on_command(b"stop", &ctx(), &mut lines);
    lines.take();
    session.on_command(b"status", &ctx(), &mut lines);
    assert_eq!(lines.take()[0], "scanning off\n");
}

#[test]
fn wardrive_session_answers_each_channel_form_when_word_is_ch() {
    let (mut session, mut lines) = subscribed();
    session.on_command(b"ch 11,1", &ctx(), &mut lines);
    assert_eq!(lines.take(), ["WD:PLAN chans=11,1\n", "scanning 11,1\n"]);
    session.on_command(b"ch", &ctx(), &mut lines);
    assert_eq!(lines.take(), [format!("supported {ALL_CHANS}\n"), "scanning 11,1\n".into()]);
    session.on_command(b"ch all", &ctx(), &mut lines);
    assert_eq!(
        lines.take(),
        [format!("WD:PLAN chans={ALL_CHANS}\n"), "scanning all 41 channels\n".into()]
    );
    // Idle, the list changes without a plan line.
    session.on_command(b"stop", &ctx(), &mut lines);
    lines.take();
    session.on_command(b"ch 6", &ctx(), &mut lines);
    assert_eq!(lines.take(), ["scanning 6\n"]);
    session.on_command(b"ch all", &ctx(), &mut lines);
    assert_eq!(lines.take(), ["scanning all 41 channels\n"]);
}

#[test]
fn wardrive_session_takes_longest_command_when_240_bytes_of_channels() {
    let mut command = b"wdstream start -wifi -ch ".to_vec();
    let mut chans = SCAN_CHANNELS.iter().cycle();
    while command.len() < COMMAND_MAX + 40 {
        command.extend_from_slice(chans.next().expect("cycles").to_string().as_bytes());
        command.push(b',');
    }
    let mut session = Session::new();
    let mut lines = Lines::default();
    assert_eq!(session.on_command(&command, &ctx(), &mut lines), Some(Ask::Start));
    let lines = lines.take();
    assert_eq!(lines.len(), 6);
    // The cut is mid-list, so the whole pool fits once, in the order given.
    assert_eq!(lines[5], format!("WD:PLAN chans={ALL_CHANS}\n"));
}

#[test]
fn wardrive_session_sends_boot_and_link_after_caps_when_subscribed() {
    let mut session = Session::new();
    let mut lines = Lines::default();
    session.on_subscribe(&ctx(), &mut lines);
    let want: Vec<_> = [
        caps_all(),
        BOOT.into(),
        LINK.into(),
        "WD:MODE mode=wifi\n".into(),
        "WD:BEGIN type=wifi interval=0 channel=auto\n".into(),
        format!("WD:PLAN chans={ALL_CHANS}\n"),
    ]
    .into_iter()
    .map(|line| (Class::Stream, line))
    .collect();
    assert_eq!(lines.take_classed(), want);
}

#[test]
fn wardrive_line_names_previous_drop_when_formatting_link() {
    let mut out = Line::new();
    let link = PhoneLink {
        connections: 3,
        last_drop: Some(LastDrop { reason: 0x13, ago_s: 12 }),
        ..PhoneLink::default()
    };
    assert!(line::link(&mut out, &link, 182));
    assert_eq!(text(&out), "WD:LINK conns=3 mtu=182 prev_disc=0x213 prev_disc_ago=12s\n");
}

#[test]
fn wardrive_line_says_none_when_no_previous_drop() {
    let mut out = Line::new();
    let link = PhoneLink { connections: 1, ..PhoneLink::default() };
    assert!(line::link(&mut out, &link, 182));
    assert_eq!(text(&out), "WD:LINK conns=1 mtu=182 prev_disc=none\n");
}

#[test]
fn wardrive_line_maps_reset_cause_to_vendor_key_when_formatting_boot() {
    let causes = [
        (ResetCause::PowerOn, "power_on"),
        (ResetCause::Software, "software"),
        (ResetCause::Watchdog, "watchdog"),
        (ResetCause::Lockup, "watchdog"),
        (ResetCause::Brownout, "brownout"),
        (ResetCause::External, "reset_pin"),
        (ResetCause::Unknown, "other"),
    ];
    let mut out = Line::new();
    for (reset, key) in causes {
        assert_eq!(line::reset_key(reset), key);
        assert!(line::boot(&mut out, &Boot { slot: "ota_1", trial: false, reset }));
        let want = format!("WD:BOOT fw=1.6.0 slot=ota_1 image=confirmed reset={key}\n");
        assert_eq!(text(&out), want, "{reset:?}");
    }
}

#[test]
fn wardrive_line_says_trial_when_image_on_trial() {
    let mut out = Line::new();
    let boot = Boot { slot: "?", trial: true, reset: ResetCause::Software };
    assert!(line::boot(&mut out, &boot));
    assert_eq!(text(&out), "WD:BOOT fw=1.6.0 slot=? image=trial reset=software\n");
}

#[test]
fn wardrive_session_sends_health_as_answer_when_period_ends() {
    let health = "WD:HEALTH up=61 heap=41000 heap_min=38000 mtu=182 txdrop=2\n";
    let (mut session, mut lines) = subscribed();
    session.on_health(&ctx(), &mut lines);
    assert_eq!(lines.take_classed(), [(Class::Answer, health.into())]);
    // Sent while not scanning too.
    session.on_command(b"stop", &ctx(), &mut lines);
    lines.take();
    session.on_health(&ctx(), &mut lines);
    assert_eq!(lines.take_classed(), [(Class::Answer, health.into())]);
}

#[test]
fn wardrive_session_answers_link_with_four_lines_when_asked() {
    let (mut session, mut lines) = subscribed();
    assert_eq!(session.on_command(b"iam a Pixel", &ctx(), &mut lines), None);
    assert!(lines.take().is_empty());
    session.on_command(b"link", &ctx(), &mut lines);
    let want = [
        "Bluetooth: connected to 4A:9F for 0h 01m 05s\n",
        "  held by Pixel (Android)\n",
        "  signal -61 dBm, MTU 182, interval 48 ms, timeout 4000 ms\n",
        "  links since boot 3, last drop 12 s ago (0x13)\n",
    ];
    let classed = lines.take_classed();
    assert!(classed.iter().all(|(class, _)| *class == Class::Answer));
    assert_eq!(classed.into_iter().map(|(_, line)| line).collect::<Vec<_>>(), want);
    // A later `iam` replaces the earlier one.
    session.on_command(b"iam i Ana's phone", &ctx(), &mut lines);
    session.on_command(b"link", &ctx(), &mut lines);
    assert_eq!(lines.take()[1], "  held by Ana's phone (iPhone)\n");
}

#[test]
fn wardrive_session_says_no_name_when_link_asked_before_iam() {
    let (mut session, mut lines) = subscribed();
    session.on_command(b"link", &ctx(), &mut lines);
    assert_eq!(lines.take()[1], "  held by an app that gave no name\n");
}

#[test]
fn wardrive_session_says_signal_unknown_when_link_has_no_reading() {
    let (mut session, mut lines) = subscribed();
    let mut ctx = ctx();
    ctx.link.rssi = None;
    session.on_command(b"link", &ctx, &mut lines);
    assert_eq!(lines.take()[2], "  signal unknown, MTU 182, interval 48 ms, timeout 4000 ms\n");
}

#[test]
fn wardrive_session_says_no_drops_when_link_has_none() {
    let (mut session, mut lines) = subscribed();
    let mut ctx = ctx();
    ctx.link.connections = 1;
    ctx.link.last_drop = None;
    session.on_command(b"link", &ctx, &mut lines);
    assert_eq!(lines.take()[3], "  links since boot 1, no drops\n");
}

#[test]
fn wardrive_session_asks_for_drop_when_drop_sent() {
    let (mut session, mut lines) = subscribed();
    assert_eq!(session.on_command(b"drop", &ctx(), &mut lines), Some(Ask::Drop));
    assert_eq!(lines.take_classed(), [(Class::Answer, "disconnecting the phone\n".into())]);
    // The disconnect releases the session; until then it scans on.
    assert!(session.scanning());
}

#[test]
fn wardrive_command_cuts_nickname_to_whole_characters_when_iam_is_long() {
    // Seventeen ASCII bytes, then `é`, two bytes: the 18th and 19th.
    let mut text = b"iam a ".to_vec();
    text.extend_from_slice(b"abcdefghijklmnopq");
    text.extend_from_slice("é".as_bytes());
    assert_eq!(
        command::parse(&text),
        Some(Command::Iam { iphone: false, nickname: b"abcdefghijklmnopq" })
    );
    // One that fits is kept whole.
    let short = "iam i café".as_bytes();
    assert_eq!(
        command::parse(short),
        Some(Command::Iam { iphone: true, nickname: "café".as_bytes() })
    );
    let long = [b'x'; 40];
    let mut text = b"iam i ".to_vec();
    text.extend_from_slice(&long);
    assert_eq!(
        command::parse(&text),
        Some(Command::Iam { iphone: true, nickname: &long[..NICKNAME_MAX] })
    );
}

#[test]
fn wardrive_command_ignores_iam_when_platform_letter_is_missing() {
    for text in [&b"iam Pixel"[..], b"iam a", b"iam x y", b"iam"] {
        assert_eq!(
            command::parse(text),
            Some(Command::Silent),
            "{}",
            String::from_utf8_lossy(text)
        );
    }
}

#[test]
fn wardrive_session_forgets_nickname_when_phone_subscribes_again() {
    let (mut session, mut lines) = subscribed();
    session.on_command(b"iam a Pixel", &ctx(), &mut lines);
    session.on_subscribe(&ctx(), &mut lines);
    lines.take();
    session.on_command(b"link", &ctx(), &mut lines);
    assert_eq!(lines.take()[1], "  held by an app that gave no name\n");
    // A release forgets it too.
    session.on_command(b"iam a Pixel", &ctx(), &mut lines);
    session.release();
    session.on_command(b"link", &ctx(), &mut lines);
    assert_eq!(lines.take()[1], "  held by an app that gave no name\n");
}

#[test]
fn wardrive_session_ends_info_with_boot_line_when_asked() {
    let mut session = Session::new();
    let mut lines = Lines::default();
    let mut ctx = ctx();
    ctx.boot = Boot { slot: "ota_1", trial: true, reset: ResetCause::Brownout };
    session.on_command(b"info", &ctx, &mut lines);
    let classed = lines.take_classed();
    assert_eq!(classed.len(), 4);
    assert_eq!(
        classed[3],
        (Class::Answer, "WD:BOOT fw=1.6.0 slot=ota_1 image=trial reset=brownout\n".into())
    );
}
