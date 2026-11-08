//! `wartui status` — ask the bridge how it is doing.
//!
//! The only command that proves the link works in both directions, which makes
//! it the first thing to reach for when records are not arriving: a bridge that
//! answers is listening, and one that does not is either wedged or not there.
//!
//! `dropped_tx` is the number worth watching: frames the bridge threw away because
//! they arrived faster than the host drained the link — a host not reading,
//! or a burst too big to queue — so observations lost between the bridge and the
//! host rather than on the node bus.
//!
//! `oversize` counts node records the bridge dropped for being longer than any node of
//! this build sends: a node on another build.
//!
//! `fan` is the carrier's lid fan: the duty the bridge runs and keeps, and its speed. `pool` is
//! the channel pool the bridge holds, which `run`'s planner deals. `slots` names the slots the
//! bridge counts a node in, as the operator reads them on its board.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use wartui_bridge::LinkEvent;
use wartui_proto::bus::{MAX_SLOTS, NodeBoard, Slot};
use wartui_proto::fan::FanStats;
use wartui_proto::link::{
    BridgeToHost, HostToBridge, QueueStat, QueueStats, RadioInfo, RadioLinkStats,
};
use wartui_proto::mac;

/// Generous enough for a bridge that is busy forwarding a fleet, short enough
/// that a wedged one is reported rather than waited on. The same figure the
/// notice quotes, so the two cannot disagree about how long was waited.
const REPLY_TIMEOUT: Duration = super::CONNECT_NOTICE_AFTER;

#[derive(ClapArgs)]
pub struct Args {
    /// The bridge, as a device path, the board's address, or the end of it (`00:08`). Detected
    /// if omitted.
    #[arg(long, value_name = "PATH|MAC")]
    pub(crate) bridge: Option<String>,

    /// Read the remembered bridge from this file instead of the default `wartui.toml`. A file
    /// that does not load is no bridge remembered. Never written; see
    /// `crates/wartui/README.md` § "Config file".
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Ask the simulator instead of hardware.
    #[arg(
        long,
        value_name = "NODES",
        num_args = 0..=1,
        default_missing_value = "3",
        value_parser = crate::sim_nodes()
    )]
    sim: Option<u8>,
}

pub async fn run(args: Args) -> Result<()> {
    let mut link =
        super::open(args.bridge.as_deref(), args.sim, super::memory(args.config.as_deref()))?;

    // The bridge announces itself on connect, so waiting for that keeps a status
    // request from going into a port nobody is listening on. The timeout is itself an
    // answer: it names the port and what to do about each cause.
    let info = match tokio::time::timeout(REPLY_TIMEOUT, wait_for_ready(&mut link)).await {
        Ok(ready) => ready?,
        Err(_) => bail!("{}", super::no_bridge_notice(args.bridge.as_deref())),
    };
    println!("bridge     {}, firmware {}", mac::full(&info.mac), info.fw_version);
    println!("board      {}", board_line(info.node_board));
    println!("{}", super::last_reset_line(&info));
    // A split bridge's radio restarts on its own, and this is where its reason shows.
    if let Some(radio) = &info.radio {
        println!("{}", radio_line(radio));
    }

    link.send_bulk(HostToBridge::GetStatus).context("queueing the status request")?;

    let status = tokio::time::timeout(REPLY_TIMEOUT, wait_for_status(&mut link))
        .await
        .context("the bridge did not answer a status request")??;

    let BridgeToHost::Status {
        rx_count,
        dropped_tx,
        uptime_ms,
        rx_oversize,
        queues,
        radio_link,
        host_uart_errors,
        fan,
        pool,
        ble,
        present,
    } = status
    else {
        unreachable!("wait_for_status only returns Status")
    };

    println!("received   {rx_count} records");
    println!("dropped    {dropped_tx} frames");
    println!("oversize   {rx_oversize} records");
    println!("uptime     {}", human_uptime(uptime_ms));
    println!("fan        {}", fan_text(fan));
    println!("pool       {pool}");
    println!("bluetooth  {}", ble_text(info.node_board, ble));
    println!("slots      {}", slots_text(info.node_board, present));
    for line in diagnostic_lines(&queues, &radio_link, host_uart_errors) {
        println!("{line}");
    }

    if dropped_tx > 0 {
        // Cumulative since the bridge booted, and one left powered with nothing
        // attached drops everything it hears — so a large number is not a fault.
        println!(
            "\n{dropped_tx} frames were discarded over those {}, because a host was \n\
             not reading or a burst arrived faster than the link could drain it. That \n\
             includes any time the bridge spent powered with nothing attached. \n\
             `wartui run` reports drops from the moment it connects, which is the \n\
             number that says whether a capture lost data.",
            human_uptime(uptime_ms)
        );
    }
    if let Some(note) = silence_note(rx_count, rx_oversize) {
        println!("\n{note}");
    }

    Ok(())
}

/// The lid fan's duty and speed, or why there is none.
pub(crate) fn fan_text(fan: Option<FanStats>) -> String {
    match fan {
        Some(FanStats { duty, rpm: Some(rpm) }) => format!("{duty}, {rpm} rpm"),
        // The bridge reads its tach for a second after boot before it has a speed.
        Some(FanStats { duty, rpm: None }) => format!("{duty}, rpm not read yet"),
        None => "no carrier, so no fan".to_owned(),
    }
}

/// The slots `present` names, as `board` labels them, or `none`.
/// The slot whose node scans Bluetooth, by its board's name, or `none`.
fn ble_text(board: NodeBoard, ble: Option<Slot>) -> String {
    ble.map_or_else(|| "none".to_owned(), |slot| board.label(slot).to_string())
}

fn slots_text(board: NodeBoard, present: u32) -> String {
    let labels: Vec<String> = (0..MAX_SLOTS as u8)
        .filter_map(Slot::from_u8)
        .filter(|slot| present & slot.bit() != 0)
        .map(|slot| board.label(slot).to_string())
        .collect();
    if labels.is_empty() { "none".to_owned() } else { labels.join(" ") }
}

/// What carries the bridge's nodes, and the slots it drives.
fn board_line(board: NodeBoard) -> String {
    if !board.is_carrier() {
        return board.to_string();
    }
    format!("{board}, {} node slots, {}", board.slot_count(), crate::text::slot_range(board))
}

/// The radio coprocessor behind a split bridge.
fn radio_line(radio: &RadioInfo) -> String {
    format!(
        "radio      {}, firmware {}, reset {:?}, last phase {:?}",
        mac::full(&radio.mac),
        radio.fw_version,
        radio.reset_cause,
        radio.last_phase
    )
}

/// One line per reported queue, the UART link's counters, then the host link's line errors.
fn diagnostic_lines(queues: &QueueStats, s: &RadioLinkStats, host_uart_errors: u32) -> Vec<String> {
    let mut lines: Vec<String> = queues.iter().map(queue_line).collect();
    lines.push(format!(
        "  radio link  packets {}  crc_fail {}  bad_magic {}  seq_gap {}  uart_errors {}  \
         radio_crc_fail {}  radio_bad_magic {}  radio_seq_gap {}  radio_uart_errors {}",
        s.packets,
        s.crc_fail,
        s.bad_magic,
        s.seq_gap,
        s.uart_errors,
        s.radio_crc_fail,
        s.radio_bad_magic,
        s.radio_seq_gap,
        s.radio_uart_errors,
    ));
    lines.push(format!("  host link   uart_errors {host_uart_errors}"));
    lines
}

fn queue_line(stat: &QueueStat) -> String {
    format!(
        "  queue {}  {}/{} bytes  dropped {}",
        stat.queue.name(),
        stat.high_water,
        stat.capacity,
        stat.dropped
    )
}

/// What to say when no node's record has reached the bridge, or `None` when one has.
///
/// An oversize record comes from a node on another build, so a bridge that has counted only
/// those has a node that needs updating.
fn silence_note(rx_count: u32, rx_oversize: u32) -> Option<String> {
    if rx_count > 0 {
        return None;
    }
    let heard = if rx_oversize > 0 {
        "A node is sending records too long for this build; update it with `wartui ota`."
    } else {
        "The bridge has had no record from a node."
    };
    Some(format!("{heard} Check the nodes are powered and wired to the bridge's node bus."))
}

/// The bridge's announcement, or why the link went down first.
pub(crate) async fn wait_for_ready(
    link: &mut wartui_bridge::LinkHandle,
) -> Result<wartui_bridge::BridgeInfo> {
    loop {
        match link.recv().await {
            Some(LinkEvent::Connected(info)) => return Ok(info),
            Some(LinkEvent::Disconnected { reason }) => bail!("{reason}"),
            Some(_) => {}
            None => bail!("the link closed"),
        }
    }
}

/// Waits for the `Status` answer, printing each fault the bridge reports meanwhile.
///
/// A bridge announces a standing fault as an `Error` once a host connects, such as a P4
/// updating its radio, and still answers the poll. The caller's timeout bounds the wait.
pub(crate) async fn wait_for_status(link: &mut wartui_bridge::LinkHandle) -> Result<BridgeToHost> {
    loop {
        match awaited(link.recv().await)? {
            Awaited::Status(status) => return Ok(*status),
            Awaited::Fault(line) => println!("{line}"),
            Awaited::Other => {}
        }
    }
}

/// What one link event means while waiting for the `Status` answer.
#[derive(Debug, PartialEq)]
enum Awaited {
    /// The answer.
    Status(Box<BridgeToHost>),
    /// A fault the bridge reported, as the line to print.
    Fault(String),
    /// Nothing `status` prints.
    Other,
}

fn awaited(event: Option<LinkEvent>) -> Result<Awaited> {
    match event {
        Some(LinkEvent::Message(status @ BridgeToHost::Status { .. })) => {
            Ok(Awaited::Status(Box::new(status)))
        }
        Some(LinkEvent::Message(BridgeToHost::Error { message })) => {
            Ok(Awaited::Fault(format!("fault      {message}")))
        }
        Some(LinkEvent::Disconnected { reason }) => bail!("{reason}"),
        Some(_) => Ok(Awaited::Other),
        None => bail!("the link closed"),
    }
}

/// Uptime in the largest unit that still reads clearly.
fn human_uptime(ms: u32) -> String {
    let seconds = ms / 1000;
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Awaited, awaited, ble_text, board_line, diagnostic_lines, fan_text, human_uptime,
        radio_line, silence_note, slots_text,
    };
    use wartui_bridge::LinkEvent;
    use wartui_proto::bus::{NodeBoard, Slot};
    use wartui_proto::fan::{FanDuty, FanStats};
    use wartui_proto::link::{
        BridgeToHost, LoopPhase, QueueId, QueueStat, QueueStats, RadioInfo, RadioLinkStats,
    };
    use wartui_proto::reset::ResetCause;

    #[test]
    fn status_prints_radio_line_when_radio_answered() {
        let radio = RadioInfo {
            mac: [0x58, 0x8c, 0x81, 0x00, 0x12, 0x34],
            fw_version: "0.1.0".try_into().expect("short"),
            reset_cause: ResetCause::Watchdog,
            last_phase: LoopPhase::RadioLink,
            uptime_ms: 72,
        };
        assert_eq!(
            radio_line(&radio),
            format!(
                "radio      {}, firmware 0.1.0, reset Watchdog, last phase RadioLink",
                wartui_proto::mac::full(&radio.mac)
            )
        );
    }

    #[test]
    fn status_prints_board_and_its_slots_when_bridge_announces() {
        assert_eq!(board_line(NodeBoard::Carrier6), "carrier-6, 6 node slots, N1 to N6");
        assert_eq!(board_line(NodeBoard::Carrier10), "carrier-10, 10 node slots, N1 to N10");
    }

    #[test]
    fn status_prints_ble_slot_by_board_label_when_bridge_reports_one() {
        let b2 = Slot::new(1, 1);
        assert_eq!(ble_text(NodeBoard::Carrier14, b2), "N9");
        assert_eq!(ble_text(NodeBoard::Carrier14, None), "none");
    }

    #[test]
    fn status_prints_board_as_no_carrier_when_bridge_has_no_carrier() {
        assert_eq!(board_line(NodeBoard::NoCarrier), "no carrier");
    }

    #[test]
    fn status_prints_queue_lines_when_queues_present() {
        let mut queues = QueueStats::new();
        queues
            .push(QueueStat {
                queue: QueueId::HostBulk,
                capacity: 262_144,
                high_water: 1_024,
                dropped: 0,
            })
            .expect("room");
        queues
            .push(QueueStat {
                queue: QueueId::NodeDown,
                capacity: 4_096,
                high_water: 3_102,
                dropped: 2,
            })
            .expect("room");
        queues
            .push(QueueStat {
                queue: QueueId::RadioUp,
                capacity: 8_192,
                high_water: 96,
                dropped: 0,
            })
            .expect("room");
        assert_eq!(
            diagnostic_lines(&queues, &RadioLinkStats::default(), 0),
            [
                "  queue host-bulk  1024/262144 bytes  dropped 0",
                "  queue node-down  3102/4096 bytes  dropped 2",
                "  queue radio-up  96/8192 bytes  dropped 0",
                RADIO_LINK_CLEAN,
                "  host link   uart_errors 0",
            ]
        );
    }

    /// The radio link's line with every counter at zero.
    const RADIO_LINK_CLEAN: &str = "  radio link  packets 0  crc_fail 0  bad_magic 0  seq_gap 0  \
         uart_errors 0  radio_crc_fail 0  radio_bad_magic 0  radio_seq_gap 0  radio_uart_errors 0";

    #[test]
    fn status_prints_radio_and_host_link_when_queues_empty() {
        assert_eq!(
            diagnostic_lines(&QueueStats::new(), &RadioLinkStats::default(), 3),
            [RADIO_LINK_CLEAN, "  host link   uart_errors 3"]
        );
    }

    #[test]
    fn status_cmd_formats_uptime_in_largest_unit_when_displaying_diagnostics() {
        assert_eq!(human_uptime(0), "0s");
        assert_eq!(human_uptime(59_999), "59s");
        assert_eq!(human_uptime(60_000), "1m 0s");
        assert_eq!(human_uptime(3_599_000), "59m 59s");
        assert_eq!(human_uptime(3_600_000), "1h 0m");
        assert_eq!(human_uptime(7_380_000), "2h 3m");
    }

    #[test]
    fn status_cmd_names_other_build_when_bridge_counted_only_oversize_records() {
        let note = silence_note(0, 3).expect("no node was heard");
        assert!(note.starts_with("A node is sending records too long"), "{note}");
        assert!(note.contains("wired to the bridge's node bus"), "{note}");
    }

    #[test]
    fn status_cmd_says_no_record_when_bridge_counted_none() {
        let note = silence_note(0, 0).expect("no node was heard");
        assert!(note.starts_with("The bridge has had no record from a node."), "{note}");
    }

    #[test]
    fn status_cmd_omits_silence_note_when_a_node_record_arrived() {
        assert_eq!(silence_note(1, 3), None);
    }

    #[test]
    fn status_cmd_prints_fault_and_keeps_waiting_when_bridge_reports_error() {
        let message = "updating the radio to the bundled image (40%)".try_into().expect("short");
        let event = LinkEvent::Message(BridgeToHost::Error { message });
        assert_eq!(
            awaited(Some(event)).expect("not an error"),
            Awaited::Fault("fault      updating the radio to the bundled image (40%)".into())
        );
    }

    #[test]
    fn status_cmd_returns_answer_when_status_arrives() {
        let status = BridgeToHost::Status {
            rx_count: 2,
            dropped_tx: 0,
            uptime_ms: 3,
            rx_oversize: 0,
            queues: QueueStats::new(),
            radio_link: RadioLinkStats::default(),
            host_uart_errors: 0,
            fan: None,
            pool: wartui_proto::plan::ChannelPool::All,
            ble: None,
            present: 0,
        };
        let event = LinkEvent::Message(status.clone());
        assert_eq!(awaited(Some(event)).expect("not an error"), Awaited::Status(Box::new(status)));
    }

    #[test]
    fn status_prints_fan_duty_and_speed_when_bridge_reports_one() {
        let duty = FanDuty::new(40).expect("in range");
        assert_eq!(fan_text(Some(FanStats { duty, rpm: Some(1830) })), "40%, 1830 rpm");
        assert_eq!(fan_text(Some(FanStats { duty, rpm: None })), "40%, rpm not read yet");
    }

    #[test]
    fn status_prints_present_slots_by_board_label_when_bridge_reports_them() {
        // A1, A2 and B1: bytes 0, 1 and 7.
        let present = 1 << 0 | 1 << 1 | 1 << 7;
        assert_eq!(slots_text(NodeBoard::Carrier6, present), "N1 N2 N4");
        assert_eq!(slots_text(NodeBoard::Carrier14, present), "N1 N2 N8");
    }

    #[test]
    fn status_prints_none_when_no_slot_is_present() {
        assert_eq!(slots_text(NodeBoard::NoCarrier, 0), "none");
    }

    #[test]
    fn status_prints_no_carrier_when_bridge_reports_no_fan() {
        assert_eq!(fan_text(None), "no carrier, so no fan");
    }

    #[test]
    fn status_cmd_fails_when_link_closes_before_answer() {
        assert!(awaited(None).is_err());
    }
}
