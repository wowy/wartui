//! `wartui sd-log` — print the bridge's SD card log as text.
//!
//! It reads log files off the card, mounted or copied, and talks to no bridge: `wartui sd log` is
//! the setting. Records decode with `wartui_proto::sd::log`, the reader the format was written
//! with, and a frame's line is `wartui sniff`'s one-liner ([`message_text`]), so a message reads
//! alike live and logged.
//!
//! **A bad record costs that record alone.** It prints as `unreadable` and reading goes on at
//! the next terminator, as the format allows. A torn tail or a card's reused blocks still leave
//! the rest of the file to read.
//!
//! **Uptime is the P4's.** The P4 knows no date, so each line gives its uptime, unwrapped from
//! the record's 32 bits by [`Clock`] from the file's [`Opened`]. Files from different boots do
//! not share a clock, so the total over several files adds each file's span rather than spanning
//! them.

use std::ffi::OsStr;
use std::ops::Range;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use wartui_proto::bus::{NodeBoard, UpMsg};
use wartui_proto::link::BridgeToHost;
use wartui_proto::sd::log::{Bad, Body, Clock, CloseReason, Entry, Opened, decode, records};
use wartui_proto::wardrive_esp::session::Class;

use crate::text::{gb, hex, message_text, thousands};

#[derive(ClapArgs)]
pub struct Args {
    /// Log files, or directories such as the card's `LOG/`, whose `NNNNNNNN.BIN` files are read in
    /// number order.
    #[arg(value_name = "PATH", required = true)]
    paths: Vec<PathBuf>,

    /// Print each frame in full under its line.
    #[arg(long)]
    verbose: bool,
}

pub fn run(args: Args) -> Result<()> {
    for line in report(&args.paths, args.verbose)? {
        println!("{line}");
    }
    Ok(())
}

/// Every line `sd-log` prints for `paths`.
fn report(paths: &[PathBuf], verbose: bool) -> Result<Vec<String>> {
    let files = log_files(paths)?;
    let mut lines = Vec::new();
    let mut total = Total::default();
    for (i, path) in files.iter().enumerate() {
        if i > 0 {
            lines.push(String::new());
        }
        let mut bytes =
            std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        let tally = render(&path.display().to_string(), &mut bytes, verbose, &mut lines);
        total.add(&tally);
    }
    if files.len() > 1 {
        lines.push(String::new());
        lines.push(total.line(files.len()));
    }
    Ok(lines)
}

/// The files `paths` name: each file as given, and each directory's log files in number order.
fn log_files(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for path in paths {
        let meta =
            std::fs::metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
        if !meta.is_dir() {
            files.push(path.clone());
            continue;
        }
        let mut numbered = Vec::new();
        for entry in
            std::fs::read_dir(path).with_context(|| format!("cannot read {}", path.display()))?
        {
            let entry = entry.with_context(|| format!("cannot read {}", path.display()))?;
            if let Some(number) = log_number(&entry.file_name()) {
                numbered.push((number, entry.path()));
            }
        }
        if numbered.is_empty() {
            bail!("{} holds no log files (NNNNNNNN.BIN)", path.display());
        }
        numbered.sort();
        files.extend(numbered.into_iter().map(|(_, path)| path));
    }
    Ok(files)
}

/// The number of a log file named `name`: eight digits and `.BIN`, in either case. A Mac's `._`
/// files beside them are not logs.
fn log_number(name: &OsStr) -> Option<u32> {
    let (stem, ext) = name.to_str()?.split_once('.')?;
    let digits = stem.len() == 8 && stem.bytes().all(|b| b.is_ascii_digit());
    (digits && ext.eq_ignore_ascii_case("BIN")).then(|| stem.parse().ok()).flatten()
}

/// What one file held.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Tally {
    records: u64,
    unreadable: u64,
    /// Records the log lost, from its `Dropped` records.
    dropped: u64,
    /// The earliest and latest uptime of a readable record.
    span: Option<(u64, u64)>,
}

impl Tally {
    fn line(&self) -> String {
        let span = self.span.map_or_else(String::new, |(first, last)| {
            format!(" · {} to {}", uptime(first), uptime(last))
        });
        format!(
            "{} records · {} unreadable · {} dropped{span}",
            thousands(self.records),
            thousands(self.unreadable),
            thousands(self.dropped),
        )
    }
}

/// What several files held.
#[derive(Debug, Default)]
struct Total {
    records: u64,
    unreadable: u64,
    dropped: u64,
    /// Each file's span, added.
    logged_ms: u64,
}

impl Total {
    fn add(&mut self, tally: &Tally) {
        self.records += tally.records;
        self.unreadable += tally.unreadable;
        self.dropped += tally.dropped;
        self.logged_ms += tally.span.map_or(0, |(first, last)| last - first);
    }

    fn line(&self, files: usize) -> String {
        format!(
            "{files} files · {} records · {} unreadable · {} dropped · {} logged",
            thousands(self.records),
            thousands(self.unreadable),
            thousands(self.dropped),
            uptime(self.logged_ms),
        )
    }
}

/// Push the lines for `file`, a log file named `name`, onto `lines`, decoding it in place.
fn render(name: &str, file: &mut [u8], verbose: bool, lines: &mut Vec<String>) -> Tally {
    let ranges: Vec<Range<usize>> = records(file).collect();
    let mut tally = Tally::default();
    let Some(first) = ranges.first() else {
        lines.push(format!("{name}  no records"));
        return tally;
    };
    // Decoded from a copy: decoding is in place, and the loop below reads the record again.
    let opened = match decode(&mut file[first.clone()].to_vec()) {
        Ok(Entry { body: Body::Opened(opened), .. }) => Some(opened),
        _ => None,
    };
    let header = opened.as_ref().map_or_else(|| "no open record".to_owned(), header_text);
    lines.push(format!("{name}  {header}"));
    if verbose && let Some(text) = opened.as_ref().and_then(|opened| opened.last_panic.as_ref()) {
        lines.push(format!("    last panic: {}", text.escape_debug()));
    }

    let mut clock = Clock::new(0);
    let mut board = NodeBoard::NoCarrier;
    for range in ranges {
        tally.records += 1;
        let len = range.len();
        let entry = match decode(&mut file[range]) {
            Ok(entry) => entry,
            Err(bad) => {
                tally.unreadable += 1;
                lines.push(format!("--:--:--.---  unreadable  {len} bytes ({})", bad_text(bad)));
                continue;
            }
        };
        match &entry.body {
            Body::Opened(opened) => clock = Clock::new(opened.opened_ms),
            Body::Frame(BridgeToHost::Ready { node_board, .. }) => board = *node_board,
            Body::Dropped(count) => tally.dropped += u64::from(*count),
            _ => {}
        }
        let at = clock.at(entry.ms);
        tally.span =
            Some(tally.span.map_or((at, at), |(first, last)| (first.min(at), last.max(at))));
        let (kind, text) = entry_text(&entry.body, board);
        lines.push(format!("{}  {kind:<10}  {text}", uptime(at)));
        if verbose && let Body::Frame(msg) | Body::Snapshot(msg) = &entry.body {
            lines.extend(detail(msg).lines().map(|line| format!("    {line}")));
        }
    }
    lines.push(tally.line());
    tally
}

/// `msg` in full: a node's heartbeat or panic decoded, since its body's bytes read as nothing,
/// and any other message as it came.
fn detail(msg: &BridgeToHost) -> String {
    if let BridgeToHost::FromNode { kind, body, .. } = msg {
        match UpMsg::decode(*kind, body) {
            // Another major or minor's layout past the prefix reads as its bytes.
            Ok(UpMsg::Heartbeat(hb)) if hb.capabilities.version.compat_with_host().drivable() => {
                return format!("{hb:#?}");
            }
            Ok(UpMsg::Panic(panic)) => return format!("{panic:#?}"),
            _ => {}
        }
    }
    format!("{msg:#?}")
}

/// The file's header: which file of which boot, the firmware, why it booted and whether the boot
/// before panicked.
fn header_text(opened: &Opened) -> String {
    let trial = if opened.trial { "on trial" } else { "confirmed" };
    let panicked = if opened.last_panic.is_some() { " · panicked last boot" } else { "" };
    format!(
        "file {}, part {} · fw {} on {}, {trial} · reset {:?}, last phase {:?}{panicked}",
        opened.file,
        opened.part,
        opened.fw_version,
        opened.slot,
        opened.reset_cause,
        opened.last_phase
    )
}

/// A record's kind column and its description.
fn entry_text(body: &Body<'_>, board: NodeBoard) -> (String, String) {
    match body {
        Body::Opened(opened) => ("open".to_owned(), open_text(opened)),
        Body::Frame(msg) => {
            let said = message_text(msg, board);
            (said.kind.to_owned(), said.text)
        }
        // Built by the log itself, not sent to a host.
        Body::Snapshot(msg) => {
            let said = message_text(msg, board);
            (format!("{}*", said.kind), said.text)
        }
        Body::PhoneLine { class, line } => {
            let class = match class {
                Class::Answer => "answer",
                Class::Stream => "stream",
            };
            ("phone ←".to_owned(), format!("{class} {}", escaped(line)))
        }
        Body::PhoneCommand(bytes) => ("phone →".to_owned(), escaped(bytes)),
        Body::Owner { input, before, after } => {
            ("owner".to_owned(), format!("{before:?} → {after:?} on {input:?}"))
        }
        Body::Dropped(count) => {
            ("dropped".to_owned(), format!("{} records lost", thousands(u64::from(*count))))
        }
        Body::Closed(reason) => {
            let text = match reason {
                CloseReason::SettingOff => "logging turned off",
                CloseReason::Rotation => "full, the next file takes over",
            };
            ("closed".to_owned(), text.to_owned())
        }
        Body::Unknown { kind, bytes } => {
            ("unknown".to_owned(), format!("kind {kind}, {} bytes", bytes.len()))
        }
    }
}

/// The `Opened` record's line: what the header leaves out.
fn open_text(opened: &Opened) -> String {
    let free = opened.free_mb.map_or_else(String::new, |free| format!(", {} GB free", gb(free)));
    let image = opened.image.map_or_else(
        || "no image digest".to_owned(),
        |digest| format!("image {}", hex(&digest[..4])),
    );
    format!("format {} · card {} GB{free} · {image}", opened.version, gb(opened.card_mb))
}

/// Why a record did not decode.
fn bad_text(bad: Bad) -> String {
    match bad {
        Bad::Cobs => "bad COBS".to_owned(),
        Bad::Short => "too short".to_owned(),
        Bad::Crc => "CRC mismatch".to_owned(),
        Bad::Frame(err) => format!("frame: {err}"),
        Bad::Malformed => "not its kind's payload".to_owned(),
    }
}

/// Bytes a phone exchanged, as text on one line.
fn escaped(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).escape_debug().to_string()
}

/// One day in milliseconds.
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// `ms` of uptime as `hh:mm:ss.mmm`, behind `Nd ` past a day.
fn uptime(ms: u64) -> String {
    let (days, rest) = (ms / DAY_MS, ms % DAY_MS);
    let clock = format!(
        "{:02}:{:02}:{:02}.{:03}",
        rest / 3_600_000,
        rest / 60_000 % 60,
        rest / 1000 % 60,
        rest % 1000
    );
    if days > 0 { format!("{days}d {clock}") } else { clock }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use wartui_proto::bus::{
        Board, Capabilities, DieTemp, Heartbeat, NODE_VERSION, NodeBus, PANIC_MAX, PanicMsg, Slot,
        UpType, Vdd,
    };
    use wartui_proto::host_owner::{Holder, Input};
    use wartui_proto::link::{
        BridgeHealth, FromNodeBody, LINK_PROTO_VERSION, LogStr, LoopPhase, ShortStr,
    };
    use wartui_proto::radio_link::RadioHealth;
    use wartui_proto::reset::ResetCause;
    use wartui_proto::sd::log::{
        FORMAT_VERSION, Kind, RECORD_MAX, encode, encode_closed, encode_dropped, encode_message,
        encode_opened, encode_owner, encode_phone_line,
    };

    use super::*;

    /// `path` as a line names it.
    fn named(path: &Path) -> String {
        path.display().to_string()
    }

    fn opened(opened_ms: u64) -> Opened {
        Opened {
            version: FORMAT_VERSION,
            file: 7,
            part: 0,
            opened_ms,
            fw_version: ShortStr::try_from("0.1.0").expect("short"),
            reset_cause: ResetCause::PowerOn,
            last_phase: LoopPhase::Unknown,
            slot: ShortStr::try_from("ota_0").expect("short"),
            trial: false,
            image: Some([0xAB; 32]),
            card_mb: 30_436,
            free_mb: Some(30_000),
            last_panic: None,
        }
    }

    fn status(uptime_ms: u32) -> BridgeToHost {
        status_with(uptime_ms, BridgeHealth::default())
    }

    fn status_with(uptime_ms: u32, health: BridgeHealth) -> BridgeToHost {
        BridgeToHost::Status {
            rx_count: 1_200,
            dropped_tx: 3,
            uptime_ms,
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

    /// Both chips' readings, as a bridge whose C6 has reported sends them.
    fn health() -> BridgeHealth {
        let radio = RadioHealth {
            die_temp: DieTemp::from_c(45),
            vdd: Vdd::from_mv(3300),
            heap_used: 60 * 1024,
            heap_free: 40 * 1024,
            heap_high_water: 70 * 1024,
        };
        BridgeHealth { p4_die_temp: DieTemp::from_c(48), radio: Some(radio) }
    }

    /// The node in N8 on a fourteen-node carrier, as its record reached the bridge.
    fn from_node(kind: UpType, body: &[u8]) -> BridgeToHost {
        BridgeToHost::FromNode {
            slot: Slot::new(1, 0).expect("B1"),
            rx_us: 0,
            kind: kind.as_u8(),
            body: FromNodeBody::from_slice(body).expect("fits"),
        }
    }

    fn heartbeat() -> BridgeToHost {
        let hb = Heartbeat {
            capabilities: Capabilities::here(Board::Esp32c5),
            mac: [0x02, 0x00, 0x5E, 0x10, 0x57, 0x84],
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
            bus: NodeBus { bad_crc: 2, handler_max_us: 14, ..NodeBus::default() },
        };
        from_node(UpType::Heartbeat, &hb.encode())
    }

    fn panic_msg() -> BridgeToHost {
        let mut body = [0; PANIC_MAX];
        let msg =
            PanicMsg { mac: [0x02, 0x00, 0x5E, 0x10, 0x57, 0x84], text: "src/bus.rs:9:5: oops" };
        let len = msg.encode_into(&mut body).expect("fits");
        from_node(UpType::Panic, &body[..len])
    }

    fn ready() -> BridgeToHost {
        BridgeToHost::Ready {
            mac: [0x02, 0x00, 0x5E, 0x10, 0x44, 0xC0],
            fw_version: ShortStr::try_from("0.1.0").expect("short"),
            reset_cause: ResetCause::PowerOn,
            last_phase: LoopPhase::Unknown,
            uptime_ms: 5_010,
            node_board: NodeBoard::Carrier14,
            radio: None,
            image: None,
        }
    }

    fn log_msg(text: &str) -> BridgeToHost {
        BridgeToHost::Log { message: LogStr::try_from(text).expect("short") }
    }

    /// One record, terminator included, as `write` encodes it.
    fn record(write: impl FnOnce(&mut [u8]) -> Option<usize>) -> Vec<u8> {
        let mut out = vec![0; RECORD_MAX];
        let n = write(&mut out).expect("encodes");
        out.truncate(n);
        out
    }

    /// A file holding every kind, a corrupt record and zero padding. Returns it and the corrupt
    /// record's length without its terminator.
    fn fixture() -> (Vec<u8>, usize) {
        let mut corrupt = record(|out| encode_message(Kind::Frame, &log_msg("torn"), 6_150, out));
        // A byte inside the record, flipped to another non-zero one.
        corrupt[3] ^= 0x01;
        assert_ne!(corrupt[3], 0);
        let cycled = BridgeToHost::PowerCycled {
            slot: Slot::new(1, 0).expect("B1"),
            outcome: wartui_proto::link::PowerOutcome::Done,
        };
        let parts = [
            record(|out| encode_opened(&opened(5_000), 5_000, out)),
            record(|out| encode_message(Kind::Frame, &ready(), 5_010, out)),
            record(|out| encode_message(Kind::Frame, &status(5_020), 5_020, out)),
            record(|out| encode_message(Kind::Snapshot, &status_with(6_000, health()), 6_000, out)),
            record(|out| encode_message(Kind::Frame, &log_msg("sd: hello"), 6_100, out)),
            corrupt.clone(),
            vec![0; 16],
            record(|out| encode_message(Kind::Frame, &cycled, 6_120, out)),
            record(|out| encode_message(Kind::Frame, &heartbeat(), 6_130, out)),
            record(|out| encode_message(Kind::Frame, &panic_msg(), 6_140, out)),
            record(|out| encode(Kind::PhoneCommand, 6_200, b"WD:START\n", out)),
            record(|out| encode_phone_line(Class::Answer, b"WD:OK", 6_300, out)),
            record(|out| {
                encode_owner(Input::PhoneSubscribed, Holder::Usb, Holder::Phone, 6_400, out)
            }),
            record(|out| encode_dropped(3, 7_000, out)),
            record(|out| encode_closed(CloseReason::Rotation, 8_000, out)),
        ];
        (parts.concat(), corrupt.len() - 1)
    }

    fn rendered(file: &[u8], verbose: bool) -> (Vec<String>, Tally) {
        let mut lines = Vec::new();
        let tally = render("LOG/00000007.BIN", &mut file.to_vec(), verbose, &mut lines);
        (lines, tally)
    }

    #[test]
    fn sd_log_prints_header_each_record_and_summary_when_file_holds_every_kind() {
        let (file, torn) = fixture();
        let (lines, tally) = rendered(&file, false);
        let expected = [
            "LOG/00000007.BIN  file 7, part 0 · fw 0.1.0 on ota_0, confirmed · reset PowerOn, \
             last phase Unknown"
                .to_owned(),
            "00:00:05.000  open        format 1 · card 29.7 GB, 29.3 GB free · image abababab"
                .to_owned(),
            format!(
                "00:00:05.010  ready       02:00:5E:10:44:C0 firmware 0.1.0 link \
                 v{LINK_PROTO_VERSION}"
            ),
            "00:00:05.020  status      1200 received, 3 dropped, 0 oversize, up 5s · P4 —"
                .to_owned(),
            "00:00:06.000  status*     1200 received, 3 dropped, 0 oversize, up 6s · P4 48 °C · \
             C6 45 °C 3.30 V heap 40K free"
                .to_owned(),
            "00:00:06.100  log         sd: hello".to_owned(),
            format!("--:--:--.---  unreadable  {torn} bytes (CRC mismatch)"),
            "00:00:06.120  power       N8 power-cycled".to_owned(),
            format!(
                "00:00:06.130  heartbeat   N8 57:84 #412 e7 power_on {NODE_VERSION};5g · 3.30 V · \
                 52 °C · refused 3/0 · heap 61K used 39K free 68K peak"
            ),
            "00:00:06.140  panic       N8 last boot: src/bus.rs:9:5: oops".to_owned(),
            "00:00:06.200  phone →     WD:START\\n".to_owned(),
            "00:00:06.300  phone ←     answer WD:OK".to_owned(),
            "00:00:06.400  owner       Usb → Phone on PhoneSubscribed".to_owned(),
            "00:00:07.000  dropped     3 records lost".to_owned(),
            "00:00:08.000  closed      full, the next file takes over".to_owned(),
            "14 records · 1 unreadable · 3 dropped · 00:00:05.000 to 00:00:08.000".to_owned(),
        ];
        assert_eq!(lines, expected);
        assert_eq!((tally.records, tally.unreadable, tally.dropped), (14, 1, 3));
    }

    #[test]
    fn sd_log_prints_message_in_full_when_verbose() {
        let file = record(|out| encode_message(Kind::Frame, &log_msg("sd: hello"), 0, out));
        let (lines, _) = rendered(&file, true);
        assert_eq!(lines[0], "LOG/00000007.BIN  no open record");
        assert_eq!(lines[1], "00:00:00.000  log         sd: hello");
        assert_eq!(lines[2], "    Log {");
        assert!(lines[3].starts_with("        message: \"sd: hello\""), "{lines:?}");
    }

    #[test]
    fn sd_log_prints_decoded_heartbeat_and_panic_when_verbose() {
        let file = [
            record(|out| encode_message(Kind::Frame, &heartbeat(), 0, out)),
            record(|out| encode_message(Kind::Frame, &panic_msg(), 10, out)),
        ]
        .concat();
        let (lines, _) = rendered(&file, true);
        assert!(lines[1].starts_with("00:00:00.000  heartbeat   B1 57:84 #412"), "{lines:?}");
        assert_eq!(lines[2], "    Heartbeat {");
        // The node's bus counters, not the body's bytes.
        assert!(lines.contains(&"            bad_crc: 2,".to_owned()), "{lines:?}");
        assert!(!lines.iter().any(|line| line.contains("body")), "{lines:?}");
        let panic = lines.iter().position(|line| line == "    PanicMsg {").expect("panic dump");
        assert!(lines[panic..].contains(&"        text: \"src/bus.rs:9:5: oops\",".to_owned()));
    }

    #[test]
    fn sd_log_prints_last_panic_when_previous_boot_panicked() {
        let text = "src/sd.rs:12:5: oops\nmore";
        let panicked = Opened {
            reset_cause: ResetCause::Software,
            last_panic: Some(text.try_into().expect("short")),
            ..opened(5_000)
        };
        let file = record(|out| encode_opened(&panicked, 5_000, out));
        let header = "LOG/00000007.BIN  file 7, part 0 · fw 0.1.0 on ota_0, confirmed · reset \
                      Software, last phase Unknown · panicked last boot";
        let (lines, _) = rendered(&file, false);
        assert_eq!(lines[0], header);
        assert!(!lines.iter().any(|line| line.contains("oops")), "{lines:?}");
        // The text on one line of its own, under the header.
        let (lines, _) = rendered(&file, true);
        assert_eq!(lines[..2], [header, "    last panic: src/sd.rs:12:5: oops\\nmore"]);
    }

    #[test]
    fn sd_log_prints_no_records_when_file_is_empty_or_all_padding() {
        for file in [Vec::new(), vec![0; 512]] {
            let (lines, tally) = rendered(&file, false);
            assert_eq!(lines, ["LOG/00000007.BIN  no records"]);
            assert_eq!(tally, Tally::default());
        }
    }

    #[test]
    fn sd_log_unwraps_uptime_when_records_pass_2_32_ms() {
        // Opened a second before the 32-bit uptime wraps; the next record carries the wrapped value.
        let opened_ms = (1u64 << 32) - 1_000;
        let file = [
            record(|out| encode_opened(&opened(opened_ms), opened_ms as u32, out)),
            record(|out| encode_dropped(1, 500, out)),
        ]
        .concat();
        let (lines, _) = rendered(&file, false);
        assert!(lines[1].starts_with("49d 17:02:46.296  open"), "{lines:?}");
        assert!(lines[2].starts_with("49d 17:02:47.796  dropped"), "{lines:?}");
    }

    #[test]
    fn sd_log_uptime_formats_days_when_past_a_day() {
        assert_eq!(uptime(0), "00:00:00.000");
        assert_eq!(uptime(3_723_456), "01:02:03.456");
        assert_eq!(uptime(DAY_MS - 1), "23:59:59.999");
        assert_eq!(uptime(DAY_MS + 1), "1d 00:00:00.001");
    }

    #[test]
    fn sd_log_reads_directory_in_number_order_when_given_a_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = record(|out| encode_opened(&opened(0), 0, out));
        for name in ["00000010.BIN", "00000002.bin", "._00000003.BIN", "README.TXT", "0000004.BIN"]
        {
            std::fs::write(dir.path().join(name), &file).expect("written");
        }
        let files = log_files(&[dir.path().to_owned()]).expect("log files");
        let names: Vec<_> = files.iter().map(|path| path.file_name().expect("named")).collect();
        assert_eq!(names, ["00000002.bin", "00000010.BIN"]);
    }

    #[test]
    fn sd_log_totals_files_when_given_several() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (file, _) = fixture();
        let empty = dir.path().join("00000009.BIN");
        std::fs::write(dir.path().join("00000007.BIN"), &file).expect("written");
        std::fs::write(&empty, []).expect("written");
        let lines = report(&[dir.path().to_owned()], false).expect("report");
        assert_eq!(lines.iter().filter(|line| line.is_empty()).count(), 2, "{lines:?}");
        assert_eq!(lines[lines.len() - 3], format!("{}  no records", named(&empty)));
        assert_eq!(
            lines.last().map(String::as_str),
            Some("2 files · 14 records · 1 unreadable · 3 dropped · 00:00:03.000 logged")
        );
    }

    #[test]
    fn sd_log_prints_no_total_when_given_one_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("00000007.BIN");
        std::fs::write(&path, fixture().0).expect("written");
        let lines = report(std::slice::from_ref(&path), false).expect("report");
        assert!(lines[0].starts_with(&format!("{}  file 7", named(&path))), "{lines:?}");
        assert!(lines.last().expect("a line").starts_with("14 records"), "{lines:?}");
    }

    #[test]
    fn sd_log_refuses_path_when_it_does_not_exist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("00000001.BIN");
        let error = report(std::slice::from_ref(&missing), false).expect_err("missing");
        assert_eq!(error.to_string(), format!("cannot read {}", missing.display()));
    }

    #[test]
    fn sd_log_refuses_directory_when_it_holds_no_log_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("notes.txt"), "x").expect("written");
        let error = report(&[dir.path().to_owned()], false).expect_err("no logs");
        assert_eq!(
            error.to_string(),
            format!("{} holds no log files (NNNNNNNN.BIN)", dir.path().display())
        );
    }
}
