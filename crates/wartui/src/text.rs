//! Number, label and message text shared by the subcommands' reports and the TUI.
//!
//! [`message_text`] is the one-liner for each bridge message. `wartui sniff` prints it for a
//! message the bridge sends now, and `wartui sd-log` for one its log kept, so a message reads
//! alike in both.

use wartui_core::engine::power_cycled_text;
use wartui_proto::bus::{Heartbeat, NodeBoard, NodeBus, Slot, UpMsg};
use wartui_proto::link::{
    BridgeHealth, BridgeToHost, LINK_PROTO_VERSION, SdLogState, SdState, SdStatus,
};
use wartui_proto::mac::{self, Mac};
use wartui_proto::plan::{ChannelSet, SCAN_CHANNELS};

/// `n` with a comma between each group of three digits.
pub(crate) fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `buffer full` and the non-zero counts of sightings a full pending buffer refused, or
/// `None` when both are zero. The TUI footer prints the same text.
pub(crate) fn buffer_full(wifi: u64, ble: u64) -> Option<String> {
    let figures: Vec<String> = [("wifi", wifi), ("ble", ble)]
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .map(|(kind, n)| format!("{kind} {}", thousands(n)))
        .collect();
    (!figures.is_empty()).then(|| format!("buffer full {}", figures.join("  ")))
}

/// The slot names `board` takes, as a range: `N1 to N10` on a carrier, `none` with no carrier.
pub(crate) fn slot_range(board: NodeBoard) -> String {
    match (board.slots().next(), board.slots().last()) {
        (Some(first), Some(last)) => format!("{} to {}", board.label(first), board.label(last)),
        _ => "none".to_owned(),
    }
}

/// The slot an operator's `name` means on `board`, or why it is refused with the names the board
/// does take.
pub(crate) fn slot_named(board: NodeBoard, name: &str) -> Result<Slot, String> {
    board.parse_label(name).ok_or_else(|| {
        format!("{name:?} is not a slot on this {board}: name one of {}", slot_range(board))
    })
}

/// The slot `name` means on a carrier `board`, or why it is refused: `no_carrier` for a bridge
/// with no carrier, and otherwise the names the board does take.
pub(crate) fn carrier_slot_named(
    board: NodeBoard,
    name: &str,
    no_carrier: &str,
) -> Result<Slot, String> {
    if !board.is_carrier() {
        return Err(no_carrier.to_owned());
    }
    slot_named(board, name)
}

/// What the operator calls a node: its slot's label, which is on the carrier for them to see, or
/// the tail of its address for a node with no slot, which has nothing else to go by.
pub(crate) fn node_name(board: NodeBoard, slot: Option<Slot>, mac: &Mac) -> String {
    slot.map_or_else(|| mac::short(mac).to_string(), |slot| board.label(slot).to_string())
}

/// Where the bridge's microSD card is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SdForm {
    /// `wartui status` and `wartui sd`: a line printed once.
    Line,
    /// The settings modal's row, redrawn while a format runs, beside a tip that says the rest.
    Row,
}

/// `mib` in GB, MiB / 1024 with one decimal: `58.9`.
pub(crate) fn gb(mib: u32) -> String {
    format!("{:.1}", f64::from(mib) / 1024.0)
}

/// The bridge's microSD card, as `form` shows it: `53.7 of 58.9 GB free`, or `no slot` for a
/// bridge with none.
pub(crate) fn sd_text(sd: Option<SdStatus>, form: SdForm) -> String {
    let Some(SdStatus { state, card_mb, free_mb, .. }) = sd else { return "no slot".to_owned() };
    let size = gb(card_mb);
    match (state, form) {
        (SdState::NoCard, _) => "no card".to_owned(),
        (SdState::NoFat32, _) => format!("{size} GB, not FAT32"),
        (SdState::Ready, _) => match free_mb {
            Some(free) => format!("{} of {size} GB free", gb(free)),
            None => format!("{size} GB, FAT32"),
        },
        (SdState::Formatting, SdForm::Line) => "formatting".to_owned(),
        (SdState::Formatting, SdForm::Row) => "formatting…".to_owned(),
        (SdState::Failed, SdForm::Line) => format!("card error ({size} GB)"),
        (SdState::Failed, SdForm::Row) => "card error".to_owned(),
    }
}

/// `bytes` in KB, MB or GB: whole KiB, or MiB and GiB with one decimal as [`gb`] gives them,
/// `1.2 MB`. The unit is the smallest whose rounded value is under 1024, so it never prints
/// `1024 KB` or `1024.0 MB`.
pub(crate) fn bytes_text(bytes: u64) -> String {
    // Ties to even, as `format!` rounds.
    let kib = (bytes as f64 / 1024.0).round_ties_even();
    if kib < 1024.0 {
        return format!("{kib:.0} KB");
    }
    let mib = bytes as f64 / (1024.0 * 1024.0);
    let tenths = (mib * 10.0).round_ties_even();
    if tenths < 10_240.0 {
        format!("{:.1} MB", tenths / 10.0)
    } else {
        format!("{:.1} GB", mib / 1024.0)
    }
}

/// `bytes` in whole KiB, rounded, as `61K`: short enough for three heap figures on one line.
pub(crate) fn kib(bytes: u32) -> String {
    // Ties to even, as `bytes_text` rounds.
    format!("{:.0}K", (f64::from(bytes) / 1024.0).round_ties_even())
}

/// A heartbeat past its node's MAC: counter, epoch, reset cause, capabilities, rail, die,
/// refusals and heap, `#412 e7 power_on 0.7.0;5g · 3.30 V · 52 °C · refused 3/0 · heap 61K used
/// 39K free 68K peak`. The refusals are Wi-Fi's then Bluetooth's as the node counts them, since
/// boot and wrapping. The heap's peak is the most in use since boot. `—` is a failed reading.
pub(crate) fn heartbeat_text(hb: &Heartbeat) -> String {
    format!(
        "#{} e{} {} {} · {} · {} · refused {}/{} · heap {} used {} free {} peak",
        hb.counter,
        hb.epoch,
        hb.reset_cause,
        hb.capabilities,
        hb.vdd,
        hb.die_temp,
        hb.wifi_refused,
        hb.ble_refused,
        kib(hb.heap_used),
        kib(hb.heap_free),
        kib(hb.heap_high_water),
    )
}

/// A node's own view of the bus since boot: its nonzero counts by name, then its two gauges,
/// `bus bad_crc 2 gaps 1 · handler max 14 µs, settle max 3 µs`. All counts zero read `bus clean`.
pub(crate) fn node_bus_text(bus: &NodeBus) -> String {
    let counts: Vec<String> = [
        ("bad_magic", bus.bad_magic),
        ("bad_crc", bus.bad_crc),
        ("bad_len", bus.bad_len),
        ("gaps", bus.gaps),
        ("early", bus.early),
        ("arm_failed", bus.arm_failed),
        ("inbox_full", bus.inbox_full),
        ("out_of_order", bus.out_of_order),
        ("repeats", bus.repeats),
        ("replaced", bus.replaced),
        ("resent", bus.resent),
        ("undelivered", bus.undelivered),
    ]
    .into_iter()
    .filter(|(_, n)| *n > 0)
    .map(|(name, n)| format!("{name} {}", thousands(u64::from(n))))
    .collect();
    let counts = if counts.is_empty() { "clean".to_owned() } else { counts.join(" ") };
    format!(
        "bus {counts} · handler max {} µs, settle max {} µs",
        thousands(u64::from(bus.handler_max_us)),
        thousands(u64::from(bus.settle_max_us)),
    )
}

/// The bridge's own chips, after a status line: ` · P4 48 °C`, then ` · C6 45 °C 3.30 V heap 40K
/// free` once the C6 has reported.
fn health_text(health: &BridgeHealth) -> String {
    let c6 = health.radio.map_or_else(String::new, |radio| {
        format!(" · C6 {} {} heap {} free", radio.die_temp, radio.vdd, kib(radio.heap_free))
    });
    format!(" · P4 {}{c6}", health.p4_die_temp)
}

/// A node's record: a heartbeat or a panic this build reads, or its type and length. A heartbeat
/// from another major or minor is not this build's layout past its prefix, even at this length.
fn node_text(label: impl core::fmt::Display, kind: u8, body: &[u8]) -> (&'static str, String) {
    match UpMsg::decode(kind, body) {
        Ok(UpMsg::Heartbeat(hb)) if hb.capabilities.version.compat_with_host().drivable() => {
            ("heartbeat", format!("{label} {} {}", mac::short(&hb.mac), heartbeat_text(&hb)))
        }
        Ok(UpMsg::Panic(panic)) => {
            ("panic", format!("{label} last boot: {}", panic.text.escape_debug()))
        }
        _ => ("node", format!("{label} type {kind}, {} bytes", body.len())),
    }
}

/// The bridge's SD log: `off`, `on · waiting for a card`, `on · writing LOG/00000007.BIN · 1.2 MB
/// written · 0 dropped · 0 errors · longest step 90 µs`, or `no slot` for a bridge with no card
/// slot. The longest step is the writer's longest stretch without yielding.
pub(crate) fn sd_log_text(sd: Option<SdStatus>) -> String {
    let Some(SdStatus { log, .. }) = sd else { return "no slot".to_owned() };
    if !log.on {
        return "off".to_owned();
    }
    let doing = match log.state {
        // The writer has not said yet.
        SdLogState::Off => return "on".to_owned(),
        SdLogState::Waiting => "waiting for a card".to_owned(),
        SdLogState::Writing => {
            let file = log.file.map_or_else(String::new, |n| format!(" LOG/{n:08}.BIN"));
            format!(
                "writing{file} · {} written · {} dropped · {} errors · longest step {} µs",
                bytes_text(log.written),
                thousands(u64::from(log.dropped)),
                thousands(u64::from(log.errors)),
                thousands(u64::from(log.busy_max_us)),
            )
        }
        SdLogState::Full => "card full".to_owned(),
        SdLogState::Refused => "volume refused".to_owned(),
        SdLogState::Failed => "stopped after a card error".to_owned(),
    };
    format!("on · {doing}")
}

/// A bridge message as one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MessageText {
    /// What the message is, in a word: `status`, `log`, `fleet`.
    pub(crate) kind: &'static str,
    /// What it says.
    pub(crate) text: String,
}

/// `msg` as one line, naming slots as `board` labels them.
pub(crate) fn message_text(msg: &BridgeToHost, board: NodeBoard) -> MessageText {
    let (kind, text) = match msg {
        BridgeToHost::Ready { mac: bridge_mac, fw_version, .. } => (
            "ready",
            format!("{} firmware {fw_version} link v{LINK_PROTO_VERSION}", mac::full(bridge_mac)),
        ),
        BridgeToHost::Status { rx_count, dropped_tx, uptime_ms, rx_oversize, health, .. } => (
            "status",
            format!(
                "{rx_count} received, {dropped_tx} dropped, {rx_oversize} oversize, up {}s{}",
                uptime_ms / 1000,
                health_text(health),
            ),
        ),
        BridgeToHost::Log { message } => ("log", message.to_string()),
        BridgeToHost::Error { message } => ("error", message.to_string()),
        BridgeToHost::Gps { bytes } => ("gps", format!("{} bytes", bytes.len())),
        BridgeToHost::Update(status) => ("update", hex(status)),
        BridgeToHost::FromNode { slot, kind, body, .. } => {
            node_text(board.label(*slot), *kind, body)
        }
        BridgeToHost::Delivered { id } => ("delivered", format!("record {id} delivered")),
        BridgeToHost::Undelivered { id, reason } => {
            ("undelivered", format!("record {id} undelivered: {reason:?}"))
        }
        BridgeToHost::PowerCycled { slot, outcome } => {
            ("power", power_cycled_text(board, *slot, *outcome))
        }
        BridgeToHost::CacheImage(status) => ("cache", hex(status)),
        BridgeToHost::Fleet(report) => (
            "fleet",
            format!("{:?}, pool {}, {} shares", report.mode, report.pool, report.shares.len()),
        ),
        BridgeToHost::SdFormatted { outcome } => ("sd", format!("formatted: {outcome:?}")),
    };
    MessageText { kind, text }
}

/// `bytes` as lowercase hex, two digits each and nothing between.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Format a set as channel numbers, the units every other tool uses.
///
/// Runs collapse to `first-last`, so a contiguous set reads `1-11`.
pub(crate) fn channel_list(set: ChannelSet) -> String {
    let mut out = String::new();
    let mut run: Option<(u8, u8)> = None;
    for idx in set.indices() {
        match run {
            Some((start, end)) if idx == end + 1 => run = Some((start, idx)),
            Some((start, end)) => {
                push_run(&mut out, start, end);
                run = Some((idx, idx));
            }
            None => run = Some((idx, idx)),
        }
    }
    if let Some((start, end)) = run {
        push_run(&mut out, start, end);
    }
    if out.is_empty() { "none".to_owned() } else { out }
}

/// Append one run to `out`: `6`, or `36-48`.
fn push_run(out: &mut String, start: u8, end: u8) {
    use core::fmt::Write as _;
    let first = SCAN_CHANNELS.get(usize::from(start)).copied().unwrap_or(0);
    let last = SCAN_CHANNELS.get(usize::from(end)).copied().unwrap_or(0);
    if !out.is_empty() {
        out.push(',');
    }
    // Writing into a `String` cannot fail.
    let _ = if first == last { write!(out, "{first}") } else { write!(out, "{first}-{last}") };
}

#[cfg(test)]
mod tests {
    use wartui_proto::bus::{NodeBoard, Slot};
    use wartui_proto::link::{SdLog, SdLogState, SdState, SdStatus};
    use wartui_proto::mac::Mac;
    use wartui_proto::plan::{ChannelPool, ChannelSet, IndexRun};

    use wartui_proto::bus::{
        Board, Capabilities, DieTemp, Heartbeat, NODE_VERSION, NodeBus, PANIC_MAX, PanicMsg,
        UpType, Vdd,
    };
    use wartui_proto::link::{BridgeHealth, BridgeToHost, FromNodeBody};
    use wartui_proto::radio_link::RadioHealth;
    use wartui_proto::reset::ResetCause;

    use super::{
        SdForm, bytes_text, channel_list, heartbeat_text, kib, message_text, node_bus_text,
        node_name, sd_log_text, sd_text, slot_named, slot_range, thousands,
    };

    const MAC: Mac = [0x02, 0x00, 0x5E, 0x10, 0x57, 0x84];

    /// A heartbeat from a node on this build, with readings.
    fn heartbeat() -> Heartbeat {
        Heartbeat {
            capabilities: Capabilities::here(Board::Esp32c5),
            mac: MAC,
            counter: 412,
            epoch: 7,
            wifi_refused: 3,
            ble_refused: 0,
            reset_cause: ResetCause::PowerOn,
            vdd: Vdd::from_mv(3300),
            die_temp: DieTemp::from_c(52),
            heap_free: 39 * 1024,
            heap_used: 61 * 1024,
            heap_high_water: 68 * 1024,
            bus: NodeBus::default(),
        }
    }

    /// A record from N8 on a fourteen-node carrier.
    fn from_node(kind: u8, body: &[u8]) -> BridgeToHost {
        BridgeToHost::FromNode {
            slot: Slot::new(1, 0).expect("B1"),
            rx_us: 0,
            kind,
            body: FromNodeBody::from_slice(body).expect("fits"),
        }
    }

    fn status(health: BridgeHealth) -> BridgeToHost {
        BridgeToHost::Status {
            rx_count: 1_200,
            dropped_tx: 3,
            uptime_ms: 123_400,
            rx_oversize: 0,
            queues: wartui_proto::link::QueueStats::new(),
            radio_link: wartui_proto::link::RadioLinkStats::default(),
            host_uart_errors: 0,
            fan: None,
            pool: wartui_proto::plan::ChannelPool::All,
            scan: wartui_proto::fleet::ScanMode::Stopped,
            ble: None,
            present: 0,
            members: Default::default(),
            sd: None,
            bluetooth: Default::default(),
            health,
        }
    }

    #[test]
    fn heartbeat_text_prints_each_field_when_node_has_readings() {
        assert_eq!(
            heartbeat_text(&heartbeat()),
            format!(
                "#412 e7 power_on {NODE_VERSION};5g · 3.30 V · 52 °C · refused 3/0 · heap 61K \
                 used 39K free 68K peak"
            )
        );
    }

    #[test]
    fn heartbeat_text_prints_dash_when_readings_failed() {
        let hb = Heartbeat { vdd: Vdd::NONE, die_temp: DieTemp::NONE, ..heartbeat() };
        assert!(heartbeat_text(&hb).contains(";5g · — · — · refused"), "{}", heartbeat_text(&hb));
    }

    #[test]
    fn kib_rounds_to_whole_kib_when_bytes_fall_between() {
        assert_eq!(kib(0), "0K");
        assert_eq!(kib(1_535), "1K");
        assert_eq!(kib(1_537), "2K");
        assert_eq!(kib(400 * 1024), "400K");
    }

    #[test]
    fn node_bus_text_reads_clean_when_counts_are_zero() {
        let bus = NodeBus { handler_max_us: 14, settle_max_us: 3, ..NodeBus::default() };
        assert_eq!(node_bus_text(&bus), "bus clean · handler max 14 µs, settle max 3 µs");
    }

    #[test]
    fn node_bus_text_names_nonzero_counts_when_bus_saw_faults() {
        let bus = NodeBus {
            bad_crc: 2,
            gaps: 1,
            undelivered: 1_200,
            handler_max_us: 14,
            settle_max_us: 3,
            ..NodeBus::default()
        };
        assert_eq!(
            node_bus_text(&bus),
            "bus bad_crc 2 gaps 1 undelivered 1,200 · handler max 14 µs, settle max 3 µs"
        );
    }

    #[test]
    fn message_text_names_heartbeat_by_slot_and_address_tail_when_body_decodes() {
        let said = message_text(
            &from_node(UpType::Heartbeat.as_u8(), &heartbeat().encode()),
            NodeBoard::Carrier14,
        );
        assert_eq!(said.kind, "heartbeat");
        assert_eq!(said.text, format!("N8 57:84 {}", heartbeat_text(&heartbeat())));
    }

    #[test]
    fn message_text_prints_type_and_length_when_heartbeat_does_not_decode() {
        let board = NodeBoard::Carrier14;
        let mut short = heartbeat().encode().to_vec();
        short.truncate(14);
        let said = message_text(&from_node(UpType::Heartbeat.as_u8(), &short), board);
        assert_eq!((said.kind, said.text.as_str()), ("node", "N8 type 1, 14 bytes"));
        // Full length, but another minor's layout past the prefix.
        let minor = NODE_VERSION.minor + 1;
        let version = wartui_proto::bus::Version { minor, ..NODE_VERSION };
        let ahead = Heartbeat {
            capabilities: Capabilities { version, ..Capabilities::here(Board::Esp32c5) },
            ..heartbeat()
        };
        let said = message_text(&from_node(UpType::Heartbeat.as_u8(), &ahead.encode()), board);
        assert_eq!((said.kind, said.text.as_str()), ("node", "N8 type 1, 63 bytes"));
    }

    #[test]
    fn message_text_names_last_boot_when_panic_decodes() {
        let mut body = [0; PANIC_MAX];
        let msg = PanicMsg { mac: MAC, text: "src/main.rs:1:1: oops\nmore" };
        let len = msg.encode_into(&mut body).expect("fits");
        let said =
            message_text(&from_node(UpType::Panic.as_u8(), &body[..len]), NodeBoard::Carrier14);
        assert_eq!(said.kind, "panic");
        // On one line, as `sd-log --verbose` prints a bridge's last panic.
        assert_eq!(said.text, "N8 last boot: src/main.rs:1:1: oops\\nmore");
    }

    #[test]
    fn message_text_appends_bridge_health_when_status_carries_it() {
        let radio = RadioHealth {
            die_temp: DieTemp::from_c(45),
            vdd: Vdd::from_mv(3300),
            heap_used: 60 * 1024,
            heap_free: 40 * 1024,
            heap_high_water: 70 * 1024,
        };
        let health = BridgeHealth { p4_die_temp: DieTemp::from_c(48), radio: Some(radio) };
        let said = message_text(&status(health), NodeBoard::Carrier14);
        assert_eq!(
            said.text,
            "1200 received, 3 dropped, 0 oversize, up 123s · P4 48 °C · C6 45 °C 3.30 V heap 40K \
             free"
        );
    }

    #[test]
    fn message_text_leaves_c6_out_when_status_has_no_radio_health() {
        let said = message_text(&status(BridgeHealth::default()), NodeBoard::Carrier14);
        assert_eq!(said.text, "1200 received, 3 dropped, 0 oversize, up 123s · P4 —");
    }

    /// A "64 GB" card: 60,350 MiB, 58.9 GB.
    fn card(state: SdState, free_mb: Option<u32>) -> Option<SdStatus> {
        Some(SdStatus::card(state, 60_350, free_mb))
    }

    /// A ready card whose log is `log`.
    fn logging(log: SdLog) -> Option<SdStatus> {
        Some(SdStatus { log, ..SdStatus::card(SdState::Ready, 60_350, Some(55_000)) })
    }

    #[test]
    fn sd_log_text_names_each_state_when_setting_is_on() {
        let on = |state| sd_log_text(logging(SdLog { on: true, state, ..SdLog::OFF }));
        assert_eq!(on(SdLogState::Waiting), "on · waiting for a card");
        assert_eq!(on(SdLogState::Full), "on · card full");
        assert_eq!(on(SdLogState::Refused), "on · volume refused");
        assert_eq!(on(SdLogState::Failed), "on · stopped after a card error");
        assert_eq!(on(SdLogState::Off), "on");
    }

    #[test]
    fn sd_log_text_names_file_and_counts_when_writing() {
        let log = SdLog {
            on: true,
            state: SdLogState::Writing,
            file: Some(7),
            written: 1_258_291,
            dropped: 0,
            errors: 1_200,
            busy_max_us: 90,
        };
        assert_eq!(
            sd_log_text(logging(log)),
            "on · writing LOG/00000007.BIN · 1.2 MB written · 0 dropped · 1,200 errors · longest step \
             90 µs"
        );
    }

    #[test]
    fn sd_log_text_reads_off_or_no_slot_when_setting_is_off_or_bridge_has_no_slot() {
        assert_eq!(sd_log_text(logging(SdLog::OFF)), "off");
        // Off is the setting, whatever the writer is still finishing.
        let closing = SdLog { state: SdLogState::Writing, file: Some(3), ..SdLog::OFF };
        assert_eq!(sd_log_text(logging(closing)), "off");
        assert_eq!(sd_log_text(None), "no slot");
    }

    #[test]
    fn bytes_text_switches_unit_when_past_1_mb_and_1024_mb() {
        assert_eq!(bytes_text(0), "0 KB");
        assert_eq!(bytes_text(12_800), "12 KB");
        assert_eq!(bytes_text(1_258_291), "1.2 MB");
        assert_eq!(bytes_text(3 * 1024 * 1024 * 1024 / 2), "1.5 GB");
    }

    #[test]
    fn bytes_text_switches_unit_when_rounding_reaches_1024() {
        assert_eq!(bytes_text(1023 * 1024), "1023 KB");
        assert_eq!(bytes_text(1_048_570), "1.0 MB", "just under 1 MiB");
        assert_eq!(bytes_text(1024 * 1024), "1.0 MB");
        assert_eq!(bytes_text(1_073_741_823), "1.0 GB", "just under 1 GiB");
    }

    #[test]
    fn sd_text_names_each_state_when_printed_as_a_line() {
        let line = |sd| sd_text(sd, SdForm::Line);
        let no_card = SdStatus::card(SdState::NoCard, 0, None);
        assert_eq!(line(None), "no slot");
        assert_eq!(line(Some(no_card)), "no card");
        assert_eq!(line(card(SdState::NoFat32, None)), "58.9 GB, not FAT32");
        assert_eq!(line(card(SdState::Ready, Some(55_000))), "53.7 of 58.9 GB free");
        assert_eq!(line(card(SdState::Ready, None)), "58.9 GB, FAT32");
        assert_eq!(line(card(SdState::Formatting, None)), "formatting");
        assert_eq!(line(card(SdState::Failed, None)), "card error (58.9 GB)");
    }

    #[test]
    fn sd_text_shortens_formatting_and_error_when_drawn_as_a_row() {
        let row = |sd| sd_text(sd, SdForm::Row);
        assert_eq!(row(card(SdState::Formatting, None)), "formatting…");
        assert_eq!(row(card(SdState::Failed, None)), "card error");
        // The rest read as the line does.
        assert_eq!(row(card(SdState::Ready, Some(55_000))), "53.7 of 58.9 GB free");
        assert_eq!(row(None), "no slot");
    }

    #[test]
    fn sd_text_rounds_to_one_decimal_when_size_is_not_whole_gb() {
        let sd = Some(SdStatus::card(SdState::NoFat32, 30_436, None));
        assert_eq!(sd_text(sd, SdForm::Line), "29.7 GB, not FAT32");
    }

    #[test]
    fn node_name_gives_slot_label_when_node_has_slot() {
        let slot = Slot::new(1, 0).expect("B1");
        assert_eq!(node_name(NodeBoard::Carrier14, Some(slot), &MAC), "N8");
    }

    #[test]
    fn node_name_gives_address_tail_when_node_has_no_slot() {
        assert_eq!(node_name(NodeBoard::Carrier14, None, &MAC), "57:84");
    }

    #[test]
    fn slot_named_refuses_name_with_range_when_board_does_not_take_it() {
        assert_eq!(slot_named(NodeBoard::Carrier14, "n14"), Ok(Slot::new(1, 6).expect("B7")));
        let refused = slot_named(NodeBoard::Carrier14, "N15").expect_err("past the board");
        assert!(refused.contains("N1 to N14"), "{refused}");
    }

    #[test]
    fn slot_range_names_first_and_last_slot_when_board_is_a_carrier() {
        assert_eq!(slot_range(NodeBoard::Carrier6), "N1 to N6");
        assert_eq!(slot_range(NodeBoard::Carrier14), "N1 to N14");
    }

    #[test]
    fn slot_range_names_none_when_there_is_no_carrier() {
        assert_eq!(slot_range(NodeBoard::NoCarrier), "none");
    }

    #[test]
    fn thousands_separates_groups_when_number_exceeds_999() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
    }

    #[test]
    fn channel_list_formats_indices_as_channel_numbers_when_given_index_runs() {
        // Indices into SCAN_CHANNELS belong to the wire format, not the screen.
        assert_eq!(channel_list(ChannelSet::from_run(IndexRun::new(0, 0))), "1");
        assert_eq!(channel_list(ChannelSet::from_run(IndexRun::new(0, 10))), "1-11");
        assert_eq!(channel_list(ChannelSet::from_run(IndexRun::new(14, 38))), "36-165");
    }

    #[test]
    fn channel_list_formats_channels_as_compact_runs_when_channel_set_is_scattered() {
        assert_eq!(channel_list(ChannelPool::Us.channels()), "1-11,36-165");
        let mut comb = ChannelSet::empty();
        for idx in [0, 2, 4, 14, 15] {
            comb.insert(idx);
        }
        assert_eq!(channel_list(comb), "1,3,5,36-40");
        assert_eq!(channel_list(ChannelSet::empty()), "none");
    }
}
