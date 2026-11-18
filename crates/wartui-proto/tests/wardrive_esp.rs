//! The WardriveESP protocol the bridge speaks to a phone app: its frames, commands, lines and
//! session.

use wartui_proto::ChannelPool;
use wartui_proto::bus::{RecordKind, Security, SightingMsg};
use wartui_proto::hci::BleTrailer;
use wartui_proto::plan::{SCAN_CHANNELS, index_of};
use wartui_proto::radio_link::{LastDrop, PhoneLink};
use wartui_proto::reset::ResetCause;
use wartui_proto::wardrive_esp::beacon::{
    self, Beacon, COMPANY, EXTRAS_CHANNELS, EXTRAS_UUID, LinkEnd, STATUS_MAX, channel_bits,
};
use wartui_proto::wardrive_esp::command::{
    self, Bt, COMMAND_MAX, ChannelList, Channels, Command, NICKNAME_MAX, parse_channels,
};
use wartui_proto::wardrive_esp::frame::{
    self, CTRL_UUID, HEADER_LEN, Inbound, RX_UUID, SERVICE_UUID, STATUS_UUID, TX_UUID, TYPE_ACK,
    TYPE_CMD, TYPE_DATA, Taken, Write,
};
use wartui_proto::wardrive_esp::line::{self, FW, FW_LEVEL, LINE_MAX, Line};
use wartui_proto::wardrive_esp::session::{Ask, Boot, Class, Ctx, Mode, Session, Sink};

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
    assert_eq!(STATUS_UUID, 0x0547_686F_7374_4553_5042_7269_6467_5354);
    assert_eq!(COMPANY, 0xFFFF);
    assert_eq!(EXTRAS_UUID, 0x5744);
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
    let wifi =
        |channels| Command::Stream { channels, unsupported_mode: false, mode: Some(Mode::Wifi) };
    assert_eq!(
        command::parse(b"wdstream start -wifi -ch 1,6,11,36"),
        Some(wifi(Some(b"1,6,11,36")))
    );
    assert_eq!(command::parse(b"wdstream start -ch 1,6 -wifi"), Some(wifi(Some(b"1,6 -wifi"))));
    assert_eq!(command::parse(b"wdstream start -wifi"), Some(wifi(None)));
    // `-ch` with no space after it names no list.
    assert_eq!(command::parse(b"wdstream start -wifi -ch"), Some(wifi(None)));
    assert_eq!(command::parse(b"wdstream start -ch ,,, -wifi"), Some(wifi(Some(b",,, -wifi"))));
}

#[test]
fn wardrive_command_flags_unsupported_mode_when_wdstream_asks_for_capture() {
    for text in [&b"wdstream start -cap"[..], b"wdstream start -promisc", b"wdstream -bt -cap"] {
        assert!(
            matches!(command::parse(text), Some(Command::Stream { unsupported_mode: true, .. })),
            "{}",
            String::from_utf8_lossy(text)
        );
    }
}

#[test]
fn wardrive_command_reads_mode_in_order_when_wdstream_names_one() {
    let mode = |text: &[u8]| match command::parse(text) {
        Some(Command::Stream { mode, unsupported_mode: false, .. }) => mode,
        other => panic!("{other:?}"),
    };
    assert_eq!(mode(b"wdstream start -bt"), Some(Mode::Bluetooth));
    // `-bt` wins over `-wifi`, wherever each is.
    assert_eq!(mode(b"wdstream start -wifi -bt"), Some(Mode::Bluetooth));
    assert_eq!(mode(b"wdstream -bt -wifi"), Some(Mode::Bluetooth));
    assert_eq!(mode(b"wdstream start -wifi"), Some(Mode::Wifi));
    assert_eq!(mode(b"wdstream start"), None);
    assert_eq!(mode(b"wdstream"), None);
    assert_eq!(
        command::parse(b"wdstream -bt -ch 1,6"),
        Some(Command::Stream {
            channels: Some(b"1,6"),
            unsupported_mode: false,
            mode: Some(Mode::Bluetooth)
        })
    );
}

#[test]
fn wardrive_command_reads_bt_forms_when_word_is_bt() {
    assert_eq!(command::parse(b"bt on"), Some(Command::Bt(Bt::On)));
    assert_eq!(command::parse(b"bt off"), Some(Command::Bt(Bt::Off)));
    assert_eq!(command::parse(b"bt on \r\n"), Some(Command::Bt(Bt::On)));
    // The rest must be exactly `on` or `off`; anything else shows the mode.
    for text in [&b"bt"[..], b"bt maybe", b"bt on now", b"bt ON", b"bt o n"] {
        assert_eq!(
            command::parse(text),
            Some(Command::Bt(Bt::Show)),
            "{}",
            String::from_utf8_lossy(text)
        );
    }
}

#[test]
fn wardrive_command_skips_spaces_when_several_follow_the_word() {
    // The WardriveESP firmware skips every space after the first word.
    assert_eq!(command::parse(b"bt  on"), Some(Command::Bt(Bt::On)));
    assert_eq!(
        command::parse(b"iam  a Pixel"),
        Some(Command::Iam { iphone: false, nickname: b"Pixel" })
    );
    assert_eq!(command::parse(b"ch   1,6"), Some(Command::Channels(Channels::Set(b"1,6"))));
    assert_eq!(
        command::parse(b"wdstream   -ch 1,6"),
        Some(Command::Stream { channels: Some(b"1,6"), unsupported_mode: false, mode: None })
    );
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
fn wardrive_command_reads_bye_when_word_is_bye() {
    assert_eq!(command::parse(b"bye"), Some(Command::Bye));
    assert_eq!(command::parse(b" bye\r\n"), Some(Command::Bye));
}

#[test]
fn wardrive_command_is_silent_when_app_sends_hb_client_time() {
    for text in [&b"hb"[..], b"client android 2.1", b"time 1763380000"] {
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
    for word in ["cap on", "csi start ch=6", "coproc", "led test", "screen", "sd", "ota begin 9"] {
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
        "WD:CAPS fw=1.7.3 chip=esp32c5 bands=2g,5g ota=0 bt=1 gps=0 cap=0 usb=0 board=wartui \
         board_name=wartui link=ble chans=1,2,3,4,5,6,7,8,9,10,11,36,40,44,48,52,56,60,64,100,\
         104,108,112,116,120,124,128,132,136,140,144,149,153,157,161,165\n"
    );
    assert_eq!(line::FW, "1.7.3");
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
    assert!(line::mode(&mut out, Mode::Wifi));
    assert_eq!(text(&out), "WD:MODE mode=wifi\n");
    assert!(line::begin(&mut out, Mode::Wifi));
    assert_eq!(text(&out), "WD:BEGIN type=wifi interval=0 channel=auto\n");
    assert!(line::mode(&mut out, Mode::Bluetooth));
    assert_eq!(text(&out), "WD:MODE mode=bt\n");
    assert!(line::begin(&mut out, Mode::Bluetooth));
    assert_eq!(text(&out), "WD:BEGIN type=bt interval=0 channel=auto\n");
    assert!(line::status_bt(&mut out, 12, 3, 0));
    assert_eq!(text(&out), "WD:STATUS bt=12 scans=3 dropped=0\n");
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

const HELP: [&str; 18] = [
    "wartui bridge, WardriveESP commands:\n",
    "  wdstream start -wifi [-ch 1,6,11]\n",
    "  wdstream start -bt\n",
    "  wdstream stop\n",
    "  start\n",
    "  stop\n",
    "  caps\n",
    "  ch\n",
    "  ch 1,6,11\n",
    "  ch all\n",
    "  bt\n",
    "  bt on\n",
    "  bt off\n",
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
            latency: 0,
            timeout_ms: 4000,
            last_drop: Some(LastDrop { reason: 0x13, ago_s: 12 }),
            heap_free: 41_000,
            heap_least: 38_000,
        },
        boot: Boot { slot: "ota_0", trial: false, reset: ResetCause::PowerOn },
    }
}

const BOOT: &str = "WD:BOOT fw=1.7.3 slot=ota_0 image=confirmed reset=power_on\n";
const LINK: &str = "WD:LINK conns=3 mtu=182 prev_disc=0x213 prev_disc_ago=12s\n";

fn caps_all() -> String {
    format!(
        "WD:CAPS fw=1.7.3 chip=esp32c5 bands=2g,5g ota=0 bt=1 gps=0 cap=0 usb=0 board=wartui \
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
    assert_eq!(session.on_subscribe(&ctx(), &mut lines), Some(Ask::Start(Mode::Wifi)));
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
    assert_eq!(session.on_subscribe(&ctx(), &mut Lines::default()), Some(Ask::Start(Mode::Wifi)));
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
    session.on_period(&ctx(), &mut lines);
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
    session.on_period(&ctx(), &mut lines);
    assert!(lines.take().is_empty());
}

#[test]
fn wardrive_session_counts_aps_since_last_status_when_period_ends() {
    let (mut session, mut lines) = subscribed();
    for _ in 0..3 {
        session.on_sighting(1, &sighting(6, b"coffee"), &mut lines);
    }
    session.on_period(&ctx(), &mut lines);
    session.on_sighting(2, &sighting(6, b"coffee"), &mut lines);
    session.on_period(&ctx(), &mut lines);
    let lines = lines.take();
    assert_eq!(lines[3], "WD:STATUS aps=3 scans=1\n");
    assert_eq!(lines[5], "WD:STATUS aps=1 scans=2\n");
}

#[test]
fn wardrive_session_sends_status_then_end_when_stopped() {
    let (mut session, mut lines) = subscribed();
    session.on_sighting(1, &sighting(6, b"coffee"), &mut lines);
    session.on_sighting(1, &sighting(6, b"coffee"), &mut lines);
    session.on_period(&ctx(), &mut lines);
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
    assert_eq!(session.on_command(b"start", &ctx(), &mut lines), Some(Ask::Start(Mode::Wifi)));
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
    session.on_period(&ctx(), &mut lines);
    session.on_sighting(2, &sighting(6, b"coffee"), &mut lines);
    session.on_command(b"stop", &ctx(), &mut lines);
    session.on_command(b"start", &ctx(), &mut lines);
    lines.take();
    session.on_command(b"status", &ctx(), &mut lines);
    assert_eq!(lines.take()[2], "aps 0 scans 0\n");
    session.on_sighting(3, &sighting(6, b"coffee"), &mut lines);
    session.on_period(&ctx(), &mut lines);
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
    assert_eq!(scan, Some(Ask::Start(Mode::Wifi)));
    let mut want = vec!["WD:ERR reason=no_supported_channels\n".to_owned()];
    want.extend(header(ALL_CHANS));
    assert_eq!(lines.take(), want);
}

#[test]
fn wardrive_session_answers_unsupported_when_mode_is_missing() {
    let mut session = Session::new();
    let mut lines = Lines::default();
    for command in
        [&b"wdstream start -cap"[..], b"wdstream start -bt -promisc", b"cap", b"ota begin 1000"]
    {
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
    session.on_period(&ctx(), &mut lines);
    lines.take();
    assert_eq!(session.on_subscribe(&ctx(), &mut lines), Some(Ask::Start(Mode::Wifi)));
    assert_eq!(lines.take(), header(ALL_CHANS));
    session.on_command(b"status", &ctx(), &mut lines);
    assert_eq!(lines.take()[2], "aps 0 scans 0\n");
}

#[test]
fn wardrive_session_classes_answers_apart_from_stream_when_scanning() {
    let (mut session, mut lines) = subscribed();
    session.on_sighting(1, &sighting(6, b"coffee"), &mut lines);
    session.on_period(&ctx(), &mut lines);
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
    session.on_period(&ctx(), &mut lines);
    session.on_sighting(1, &sighting(6, b"coffee"), &mut lines);
    lines.take();
    session.on_command(b"status", &ctx(), &mut lines);
    assert_eq!(
        lines.take(),
        [
            "mode Wi-Fi\n",
            "scanning on\n",
            "aps 2 scans 1\n",
            "channels 6,1\n",
            "mtu 182 dropped 2\n"
        ]
    );
    session.on_command(b"info", &ctx(), &mut lines);
    assert_eq!(
        lines.take(),
        [
            "wartui bridge 44:C0 fw 0.9.0\n",
            "WardriveESP protocol 1.7.3\n",
            "uptime 61 s nodes 3\n",
            BOOT
        ]
    );
    session.on_command(b"stop", &ctx(), &mut lines);
    lines.take();
    session.on_command(b"status", &ctx(), &mut lines);
    assert_eq!(lines.take()[1], "scanning off\n");
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
    assert_eq!(session.on_command(&command, &ctx(), &mut lines), Some(Ask::Start(Mode::Wifi)));
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
fn wardrive_line_formats_conn_golden_when_timing_given() {
    let mut out = Line::new();
    // The interval rounds down to whole milliseconds.
    let link =
        PhoneLink { interval_us: 48_750, latency: 2, timeout_ms: 5000, ..PhoneLink::default() };
    assert!(line::conn(&mut out, &link));
    assert_eq!(text(&out), "WD:CONN itvl_ms=48 latency=2 timeout_ms=5000 status=0\n");
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
        let want = format!("WD:BOOT fw=1.7.3 slot=ota_1 image=confirmed reset={key}\n");
        assert_eq!(text(&out), want, "{reset:?}");
    }
}

#[test]
fn wardrive_line_says_trial_when_image_on_trial() {
    let mut out = Line::new();
    let boot = Boot { slot: "?", trial: true, reset: ResetCause::Software };
    assert!(line::boot(&mut out, &boot));
    assert_eq!(text(&out), "WD:BOOT fw=1.7.3 slot=? image=trial reset=software\n");
}

#[test]
fn wardrive_session_sends_nothing_when_link_timing_matches_subscribe() {
    let (mut session, mut lines) = subscribed();
    session.on_link(&ctx().link, &mut lines);
    assert!(lines.take().is_empty());
}

#[test]
fn wardrive_session_sends_conn_once_when_link_timing_changes() {
    let base = ctx().link;
    let changes = [
        PhoneLink { interval_us: 30_000, ..base },
        PhoneLink { latency: 4, ..base },
        PhoneLink { timeout_ms: 6000, ..base },
    ];
    let wants = [
        "WD:CONN itvl_ms=30 latency=0 timeout_ms=4000 status=0\n",
        "WD:CONN itvl_ms=48 latency=4 timeout_ms=4000 status=0\n",
        "WD:CONN itvl_ms=48 latency=0 timeout_ms=6000 status=0\n",
    ];
    for (link, want) in changes.iter().zip(wants) {
        let (mut session, mut lines) = subscribed();
        session.on_link(link, &mut lines);
        assert_eq!(lines.take_classed(), [(Class::Answer, want.into())], "{link:?}");
        // The same report again is no change.
        session.on_link(link, &mut lines);
        assert!(lines.take().is_empty(), "{link:?}");
    }
}

#[test]
fn wardrive_session_sends_conn_when_link_timing_changes_while_stopped() {
    let (mut session, mut lines) = subscribed();
    session.on_command(b"stop", &ctx(), &mut lines);
    lines.take();
    session.on_link(&PhoneLink { latency: 4, ..ctx().link }, &mut lines);
    assert_eq!(lines.take().len(), 1);
}

#[test]
fn wardrive_session_sends_nothing_when_only_signal_or_heap_changes() {
    let (mut session, mut lines) = subscribed();
    let base = ctx().link;
    session.on_link(&PhoneLink { rssi: Some(-80), ..base }, &mut lines);
    session.on_link(&PhoneLink { heap_free: 1, heap_least: 1, ..base }, &mut lines);
    session.on_link(&PhoneLink { rssi: None, connected_s: 900, ..base }, &mut lines);
    assert!(lines.take().is_empty());
}

#[test]
fn wardrive_session_takes_new_baseline_when_phone_subscribes_again() {
    let (mut session, mut lines) = subscribed();
    let mut ctx = ctx();
    ctx.link.interval_us = 30_000;
    session.on_subscribe(&ctx, &mut lines);
    lines.take();
    session.on_link(&ctx.link, &mut lines);
    assert!(lines.take().is_empty());
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
        (Class::Answer, "WD:BOOT fw=1.7.3 slot=ota_1 image=trial reset=brownout\n".into())
    );
}

/// A sink that keeps nothing.
struct Discard;

impl Sink for Discard {
    fn line(&mut self, _: Class, _: &[u8]) {}
}

/// A beacon scanning, with 0x0102 lines, from an iPhone named `Al`.
fn named_beacon() -> Beacon {
    Beacon {
        scanning: true,
        bluetooth: false,
        count: 0x0102,
        iphone: true,
        nickname: wartui_proto::heapless::Vec::from_slice(b"Al").unwrap(),
        link_end: LinkEnd::Released,
        channels: 0,
    }
}

/// The status bytes of `beacon`, for the bridge `44:C0`.
fn status_bytes(beacon: &Beacon, connected: bool) -> Vec<u8> {
    let mut out = [0; STATUS_MAX];
    let len = beacon::status(&mut out, beacon, [0x44, 0xC0], connected);
    out[..len].to_vec()
}

#[test]
fn wardrive_line_spells_fw_level_when_built() {
    // `WD:CAPS` and the status beacon name one level.
    let (major, minor, patch) = FW_LEVEL;
    assert_eq!(FW, format!("{major}.{minor}.{patch}"));
}

#[test]
fn wardrive_beacon_sends_fixed_status_golden_when_free() {
    // The free board's scan response: no nickname, and no iPhone bit without a central.
    #[rustfmt::skip]
    let want = [
        0x57,       // `W`
        0b10,       // scanning
        0x80,       // temperature unknown
        0x02, 0x01, // count, LE
        0x17, 0x03, // level 1.7, patch 3
        0x44, 0xC0, // the bridge's last two octets
    ];
    assert_eq!(status_bytes(&named_beacon(), false), want);
    assert_eq!(status_bytes(&Beacon::default(), false), [0x57, 0, 0x80, 0, 0, 0x17, 3, 0x44, 0xC0]);
}

#[test]
fn wardrive_beacon_sends_status_with_nickname_golden_when_connected() {
    let iphone = named_beacon();
    // Connected, scanning and an iPhone, then the nickname.
    let want = [0x57, 0x83, 0x80, 0x02, 0x01, 0x17, 0x03, 0x44, 0xC0, b'A', b'l'];
    assert_eq!(status_bytes(&iphone, true), want);
    let android = Beacon { iphone: false, ..named_beacon() };
    let want = [0x57, 0x03, 0x80, 0x02, 0x01, 0x17, 0x03, 0x44, 0xC0, b'A', b'l'];
    assert_eq!(status_bytes(&android, true), want);
    // A central that gave no name, such as a wartui host.
    assert_eq!(status_bytes(&Beacon::default(), true), [0x57, 1, 0x80, 0, 0, 0x17, 3, 0x44, 0xC0]);
}

#[test]
fn wardrive_beacon_fills_status_max_when_nickname_is_whole() {
    let nickname = wartui_proto::heapless::Vec::from_slice(&[b'n'; NICKNAME_MAX]).unwrap();
    let beacon = Beacon { nickname, ..Beacon::default() };
    assert_eq!(status_bytes(&beacon, true).len(), STATUS_MAX);
    assert_eq!(STATUS_MAX, 27);
}

#[test]
fn wardrive_beacon_sends_extras_golden_when_pool_is_us() {
    let beacon = Session::new().beacon(ChannelPool::Us, LinkEnd::None);
    // 1-11, then 36-165.
    assert_eq!(beacon.channels, 0x7F_FFFF_C7FF);
    assert_eq!(beacon::extras(&beacon, false), [0, 0, 0xFF, 0xFF, 0xC7, 0xFF]);
    assert_eq!(beacon::extras(&beacon, true), [1, 0xFF, 0x7F, 0, 0, 0]);
    let lost = Beacon { link_end: LinkEnd::Lost, ..beacon };
    assert_eq!(beacon::extras(&lost, false)[0], 0b100);
    assert_eq!(beacon::extras(&lost, true)[0], 0b101);
}

#[test]
fn wardrive_beacon_sends_extras_golden_when_list_is_1_6_11_36() {
    let (mut session, mut lines) = subscribed();
    session.on_command(b"ch 1,6,11,36", &ctx(), &mut lines);
    let beacon = session.beacon(ChannelPool::All, LinkEnd::Released);
    assert_eq!(beacon.channels, 0x4421);
    assert_eq!(beacon::extras(&beacon, false), [0b10, 0, 0xFF, 0x21, 0x44, 0x00]);
    assert_eq!(beacon::extras(&beacon, true), [0b11, 0, 0, 0, 0, 0]);
}

#[test]
fn wardrive_beacon_sends_top_bits_when_pool_is_all() {
    let beacon = Session::new().beacon(ChannelPool::All, LinkEnd::None);
    // Every entry but channel 14.
    assert_eq!(beacon.channels, (1 << 42) - 1 - (1 << 13));
    assert_eq!(beacon::extras(&beacon, true), [1, 0xFF, 0xFF, 0b11, 0, 0]);
}

#[test]
fn wardrive_beacon_sets_bit_of_table_entry_when_channel_is_listed() {
    assert_eq!(channel_bits(&[1]), 1);
    assert_eq!(channel_bits(&[14]), 1 << 13);
    assert_eq!(channel_bits(&[36]), 1 << 14);
    assert_eq!(channel_bits(&[177]), 1 << 41);
    assert_eq!(EXTRAS_CHANNELS.len(), 42);
}

#[test]
fn wardrive_beacon_sets_no_bit_when_channel_is_outside_table() {
    for channel in [0, 15, 34, 38, 178, 255] {
        assert_eq!(channel_bits(&[channel]), 0, "channel {channel}");
    }
    assert_eq!(channel_bits(&[34, 6]), 1 << 5);
}

#[test]
fn wardrive_session_saturates_beacon_count_when_period_holds_over_u16() {
    let mut session = Session::new();
    session.on_subscribe(&ctx(), &mut Discard);
    for _ in 0..=u32::from(u16::MAX) {
        session.on_sighting(1, &sighting(6, b"coffee"), &mut Discard);
    }
    session.on_period(&ctx(), &mut Discard);
    assert_eq!(session.beacon(ChannelPool::All, LinkEnd::None).count, u16::MAX);
}

#[test]
fn wardrive_session_fills_beacon_when_phone_names_itself_and_period_ends() {
    let (mut session, mut lines) = subscribed();
    let fresh = session.beacon(ChannelPool::All, LinkEnd::Lost);
    let all = Session::new().beacon(ChannelPool::All, LinkEnd::None).channels;
    assert_eq!(
        fresh,
        Beacon { scanning: true, link_end: LinkEnd::Lost, channels: all, ..Beacon::default() }
    );
    session.on_command(b"iam i Al", &ctx(), &mut lines);
    for _ in 0..3 {
        session.on_sighting(1, &sighting(6, b"coffee"), &mut lines);
    }
    // Counted only once the period ends.
    assert_eq!(session.beacon(ChannelPool::All, LinkEnd::None).count, 0);
    session.on_period(&ctx(), &mut lines);
    let beacon = session.beacon(ChannelPool::All, LinkEnd::None);
    assert_eq!((beacon.count, beacon.iphone, &beacon.nickname[..]), (3, true, &b"Al"[..]));
    session.on_command(b"ch 1,6,11", &ctx(), &mut lines);
    assert_eq!(session.beacon(ChannelPool::All, LinkEnd::None).channels, 0x421);
    session.on_command(b"stop", &ctx(), &mut lines);
    assert!(!session.beacon(ChannelPool::All, LinkEnd::None).scanning);
}

#[test]
fn wardrive_session_says_bye_when_app_sends_it() {
    let (mut session, mut lines) = subscribed();
    assert!(!session.said_bye());
    assert_eq!(session.on_command(b"bye", &ctx(), &mut lines), None);
    assert!(lines.take().is_empty());
    assert!(session.said_bye());
    // It is the session's: a subscribe clears it, and so does a release.
    session.on_subscribe(&ctx(), &mut lines);
    assert!(!session.said_bye());
    session.on_command(b"bye", &ctx(), &mut lines);
    session.release();
    assert!(!session.said_bye());
}

/// An advertiser's address, MSB first.
const BT_MAC: [u8; 6] = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];

/// The trailer bytes for `trailer`.
fn trailer_bytes(trailer: BleTrailer) -> Vec<u8> {
    let mut out = [0; BleTrailer::MAX];
    let len = trailer.encode(&mut out);
    out[..len].to_vec()
}

/// A Bluetooth record from [`BT_MAC`] at -70 dBm, named `name`, with the trailer `ext`.
fn ble_sighting<'a>(name: &'a [u8], ext: &'a [u8]) -> SightingMsg<'a> {
    SightingMsg {
        kind: RecordKind::Ble,
        bssid: BT_MAC,
        channel: 0,
        rssi: -70,
        security: Security::Ble,
        ssid: name,
        ext,
    }
}

/// The trailer of a random, connectable advertiser with company identifier `0x004C`.
fn apple_trailer() -> Vec<u8> {
    trailer_bytes(BleTrailer { random: true, connectable: true, mfgr: Some(0x004C) })
}

const BT_LINE: &str =
    "WD:BT ts=61500 mac=02:11:22:33:44:55 type=random rssi=-70 name_hex=4d79 mfg=004C conn=1\n";

/// The lines a Bluetooth scan starts with.
fn bt_header() -> Vec<String> {
    vec!["WD:MODE mode=bt\n".into(), "WD:BEGIN type=bt interval=0 channel=auto\n".into()]
}

/// A session the phone has subscribed to, switched to Bluetooth, and the lines that took,
/// drained.
fn bluetooth() -> (Session, Lines) {
    let (mut session, mut lines) = subscribed();
    assert_eq!(session.on_command(b"bt on", &ctx(), &mut lines), Some(Ask::Start(Mode::Bluetooth)));
    lines.take();
    (session, lines)
}

#[test]
fn wardrive_line_formats_bt_golden_when_trailer_decodes() {
    let mut out = Line::new();
    let ext = apple_trailer();
    let record = ble_sighting(b"My", &ext);
    let trailer = BleTrailer::decode(record.ext).expect("decodes");
    assert!(line::bt(&mut out, 61_500, &record, &trailer));
    assert_eq!(text(&out), BT_LINE);
    let bare = BleTrailer { random: false, connectable: false, mfgr: None };
    let ext = trailer_bytes(bare);
    assert!(line::bt(&mut out, 61_500, &ble_sighting(b"", &ext), &bare));
    assert_eq!(
        text(&out),
        "WD:BT ts=61500 mac=02:11:22:33:44:55 type=public rssi=-70 name_hex= mfg= conn=0\n"
    );
}

#[test]
fn wardrive_line_fits_bt_when_name_is_longest() {
    let mut out = Line::new();
    let name = [0xFFu8; wartui_proto::hci::BLE_NAME_MAX];
    let ext = apple_trailer();
    let record = SightingMsg { rssi: -128, ..ble_sighting(&name, &ext) };
    let trailer = BleTrailer::decode(&ext).expect("decodes");
    assert!(line::bt(&mut out, u32::MAX, &record, &trailer));
    assert!(out.len() <= LINE_MAX);
}

#[test]
fn wardrive_session_switches_to_bluetooth_when_bt_on_while_scanning_wifi() {
    let (mut session, mut lines) = subscribed();
    session.on_sighting(1, &sighting(6, b"coffee"), &mut lines);
    lines.take();
    assert_eq!(session.on_command(b"bt on", &ctx(), &mut lines), Some(Ask::Start(Mode::Bluetooth)));
    let mut want = vec![
        (Class::Answer, "WD:MODE mode=bt\n".to_owned()),
        (Class::Stream, "WD:STATUS aps=1 scans=0\n".into()),
        (Class::Stream, "WD:END reason=stop\n".into()),
    ];
    want.extend(bt_header().into_iter().map(|line| (Class::Stream, line)));
    assert_eq!(lines.take_classed(), want);
    assert_eq!(session.mode(), Mode::Bluetooth);
    assert!(session.scanning());
}

#[test]
fn wardrive_session_switches_to_wifi_when_bt_off_while_scanning_bluetooth() {
    let (mut session, mut lines) = bluetooth();
    let ext = apple_trailer();
    session.on_sighting(61_500, &ble_sighting(b"My", &ext), &mut lines);
    lines.take();
    assert_eq!(session.on_command(b"bt off", &ctx(), &mut lines), Some(Ask::Start(Mode::Wifi)));
    let mut want = vec![
        "WD:MODE mode=wifi\n".to_owned(),
        "WD:STATUS bt=1 scans=0 dropped=0\n".into(),
        "WD:END reason=stop\n".into(),
    ];
    want.extend(header(ALL_CHANS));
    assert_eq!(lines.take(), want);
    assert_eq!(session.mode(), Mode::Wifi);
}

#[test]
fn wardrive_session_answers_mode_and_asks_nothing_when_bt_on_twice() {
    let (mut session, mut lines) = bluetooth();
    assert_eq!(session.on_command(b"bt on", &ctx(), &mut lines), None);
    assert_eq!(lines.take_classed(), [(Class::Answer, "WD:MODE mode=bt\n".into())]);
    assert!(session.scanning());
}

#[test]
fn wardrive_session_starts_bluetooth_when_bt_on_while_stopped() {
    let (mut session, mut lines) = subscribed();
    session.on_command(b"stop", &ctx(), &mut lines);
    lines.take();
    assert_eq!(session.on_command(b"bt on", &ctx(), &mut lines), Some(Ask::Start(Mode::Bluetooth)));
    assert_eq!(lines.take(), bt_header());
    // A stop and a start keep the mode.
    session.on_command(b"stop", &ctx(), &mut lines);
    assert_eq!(lines.take(), ["WD:STATUS bt=0 scans=0 dropped=0\n", "WD:END reason=stop\n"]);
    assert_eq!(session.on_command(b"start", &ctx(), &mut lines), Some(Ask::Start(Mode::Bluetooth)));
    assert_eq!(
        lines.take_classed(),
        bt_header().into_iter().map(|l| (Class::Stream, l)).collect::<Vec<_>>()
    );
}

#[test]
fn wardrive_session_refuses_bluetooth_when_fleet_has_one_node() {
    let (mut session, mut lines) = subscribed();
    let lone = Ctx { nodes: 1, ..ctx() };
    for command in [&b"bt on"[..], b"wdstream start -bt"] {
        assert_eq!(session.on_command(command, &lone, &mut lines), None);
        assert_eq!(
            lines.take_classed(),
            [(Class::Answer, "WD:ERR reason=bt_needs_two_nodes\n".into())]
        );
    }
    assert_eq!(session.mode(), Mode::Wifi);
    assert!(session.scanning());
    // Stopped, it stays stopped.
    session.on_command(b"stop", &lone, &mut lines);
    lines.take();
    assert_eq!(session.on_command(b"bt on", &lone, &mut lines), None);
    assert!(!session.scanning());
}

#[test]
fn wardrive_session_answers_mode_when_bt_has_no_on_or_off() {
    let (mut session, mut lines) = subscribed();
    for command in [&b"bt"[..], b"bt maybe"] {
        assert_eq!(session.on_command(command, &ctx(), &mut lines), None);
        assert_eq!(lines.take_classed(), [(Class::Answer, "mode Wi-Fi\n".into())]);
    }
    session.on_command(b"bt on", &ctx(), &mut lines);
    lines.take();
    session.on_command(b"bt", &ctx(), &mut lines);
    assert_eq!(lines.take(), ["mode Bluetooth\n"]);
}

#[test]
fn wardrive_session_starts_bluetooth_when_wdstream_asks_for_bt() {
    let mut session = Session::new();
    let mut lines = Lines::default();
    let scan = session.on_command(b"wdstream start -bt", &ctx(), &mut lines);
    assert_eq!(scan, Some(Ask::Start(Mode::Bluetooth)));
    assert_eq!(lines.take(), bt_header());
}

#[test]
fn wardrive_session_sets_list_then_switches_when_wdstream_has_bt_and_ch() {
    let (mut session, mut lines) = subscribed();
    let scan = session.on_command(b"wdstream -bt -ch 1,6", &ctx(), &mut lines);
    assert_eq!(scan, Some(Ask::Start(Mode::Bluetooth)));
    let mut want = vec![
        "WD:PLAN chans=1,6\n".to_owned(),
        "WD:MODE mode=bt\n".into(),
        "WD:STATUS aps=0 scans=0\n".into(),
        "WD:END reason=stop\n".into(),
    ];
    want.extend(bt_header());
    assert_eq!(lines.take(), want);
    session.on_command(b"ch", &ctx(), &mut lines);
    assert_eq!(lines.take()[1], "scanning 1,6\n");
}

#[test]
fn wardrive_session_switches_to_wifi_when_wdstream_asks_for_wifi_in_bluetooth() {
    let (mut session, mut lines) = bluetooth();
    let scan = session.on_command(b"wdstream start -wifi", &ctx(), &mut lines);
    assert_eq!(scan, Some(Ask::Start(Mode::Wifi)));
    let mut want = vec![
        "WD:MODE mode=wifi\n".to_owned(),
        "WD:STATUS bt=0 scans=0 dropped=0\n".into(),
        "WD:END reason=stop\n".into(),
    ];
    want.extend(header(ALL_CHANS));
    assert_eq!(lines.take(), want);
}

#[test]
fn wardrive_session_changes_nothing_when_wdstream_asks_for_bt_and_capture() {
    let (mut session, mut lines) = subscribed();
    assert_eq!(session.on_command(b"wdstream start -bt -cap", &ctx(), &mut lines), None);
    assert_eq!(lines.take_classed(), [(Class::Answer, "WD:ERR reason=unsupported\n".into())]);
    assert_eq!(session.mode(), Mode::Wifi);
    assert!(session.scanning());
}

#[test]
fn wardrive_session_sends_only_mode_kind_when_records_arrive() {
    let ext = apple_trailer();
    let (mut session, mut lines) = bluetooth();
    session.on_sighting(61_500, &sighting(6, b"coffee"), &mut lines);
    assert!(lines.take().is_empty());
    session.on_sighting(61_500, &ble_sighting(b"My", &ext), &mut lines);
    assert_eq!(lines.take_classed(), [(Class::Stream, BT_LINE.into())]);
    // The channel list does not apply: a Bluetooth record's channel is 0.
    session.on_command(b"ch 1,6", &ctx(), &mut lines);
    lines.take();
    session.on_sighting(61_500, &ble_sighting(b"My", &ext), &mut lines);
    assert_eq!(lines.take(), [BT_LINE]);
    session.on_command(b"bt off", &ctx(), &mut lines);
    lines.take();
    session.on_sighting(61_500, &ble_sighting(b"My", &ext), &mut lines);
    assert!(lines.take().is_empty());
    session.on_sighting(61_500, &sighting(6, b"coffee"), &mut lines);
    assert_eq!(lines.take(), [ap_line(61_500, 6)]);
}

#[test]
fn wardrive_session_skips_bt_when_trailer_does_not_decode() {
    let (mut session, mut lines) = bluetooth();
    for ext in [&[0x03, 0x4C][..], &[], &[0x03, 0x4C, 0x00, 0x01]] {
        session.on_sighting(1, &ble_sighting(b"My", ext), &mut lines);
    }
    assert!(lines.take().is_empty());
    session.on_period(&ctx(), &mut lines);
    assert_eq!(lines.take(), ["WD:STATUS bt=0 scans=1 dropped=0\n"]);
}

#[test]
fn wardrive_session_reports_dropped_change_when_bluetooth_period_ends() {
    let (mut session, mut lines) = bluetooth();
    let ext = apple_trailer();
    // The scan started at `ctx().dropped`, 2.
    session.on_sighting(1, &ble_sighting(b"My", &ext), &mut lines);
    session.on_sighting(1, &ble_sighting(b"My", &ext), &mut lines);
    lines.take();
    session.on_period(&Ctx { dropped: 5, ..ctx() }, &mut lines);
    assert_eq!(
        lines.take_classed(),
        [(Class::Stream, "WD:STATUS bt=2 scans=1 dropped=3\n".into())]
    );
    session.on_period(&Ctx { dropped: 6, ..ctx() }, &mut lines);
    assert_eq!(lines.take(), ["WD:STATUS bt=0 scans=2 dropped=1\n"]);
    // A stop reports the change since the last period too.
    session.on_command(b"stop", &Ctx { dropped: 10, ..ctx() }, &mut lines);
    assert_eq!(lines.take(), ["WD:STATUS bt=0 scans=2 dropped=4\n", "WD:END reason=stop\n"]);
}

#[test]
fn wardrive_session_returns_to_wifi_when_phone_subscribes_after_bluetooth() {
    let (mut session, mut lines) = bluetooth();
    assert_eq!(session.on_subscribe(&ctx(), &mut lines), Some(Ask::Start(Mode::Wifi)));
    assert_eq!(lines.take(), header(ALL_CHANS));
    assert_eq!(session.mode(), Mode::Wifi);
    // A release resets it too.
    session.on_command(b"bt on", &ctx(), &mut lines);
    assert_eq!(session.release(), Some(Ask::Stop));
    assert_eq!(session.mode(), Mode::Wifi);
}

#[test]
fn wardrive_session_answers_status_with_bt_counts_when_in_bluetooth() {
    let (mut session, mut lines) = bluetooth();
    let ext = apple_trailer();
    session.on_sighting(1, &ble_sighting(b"My", &ext), &mut lines);
    session.on_period(&ctx(), &mut lines);
    lines.take();
    session.on_command(b"status", &ctx(), &mut lines);
    assert_eq!(
        lines.take(),
        [
            "mode Bluetooth\n",
            "scanning on\n",
            "bt 1 scans 1\n",
            format!("channels {ALL_CHANS}\n").as_str(),
            "mtu 182 dropped 2\n"
        ]
    );
}

#[test]
fn wardrive_beacon_sets_mode_bits_when_session_is_bluetooth() {
    let beacon = Beacon { scanning: true, bluetooth: true, ..Beacon::default() };
    assert_eq!(status_bytes(&beacon, true)[1], 0x07);
    let free = Beacon { bluetooth: true, ..Beacon::default() };
    assert_eq!(status_bytes(&free, false)[1], 0x04);
    let (mut session, mut lines) = subscribed();
    assert!(!session.beacon(ChannelPool::All, LinkEnd::None).bluetooth);
    session.on_command(b"bt on", &ctx(), &mut lines);
    let beacon = session.beacon(ChannelPool::All, LinkEnd::None);
    assert!(beacon.bluetooth && beacon.scanning);
    // The count is the last period's `WD:BT` lines.
    let ext = apple_trailer();
    session.on_sighting(1, &ble_sighting(b"My", &ext), &mut lines);
    session.on_period(&ctx(), &mut lines);
    assert_eq!(session.beacon(ChannelPool::All, LinkEnd::None).count, 1);
}
