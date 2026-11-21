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
//! the channel pool the bridge holds, which its planner deals. `slots` names the slots the
//! bridge counts a node in, as the operator reads them on its board. `scan`, `bluetooth` and the
//! `share` lines are the planner's: its mode, the node it picked for Bluetooth, and each node's
//! share, from the `Fleet` report the bridge sends behind every `Status`.
//!
//! `host` and `pairing` are the Bluetooth host link, labeled apart from `bluetooth`, which is the
//! node scanning Bluetooth. `host` is who the bridge serves, USB or a Bluetooth host, and the
//! Bluetooth host connected, if any. `pairing` is the pairing window and the bonds the bridge
//! keeps. The C6's pairing counters go with the other diagnostics.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use wartui_bridge::{BridgeInfo, DisconnectCause, LinkEvent, LinkHandle};
use wartui_proto::bus::{MAX_SLOTS, NodeBoard, Slot};
use wartui_proto::fan::FanStats;
use wartui_proto::fleet::{Report, ScanMode};
use wartui_proto::link::{
    BluetoothStatus, BridgeToHost, Central, HostOwner, HostToBridge, PairWindow, QueueStat,
    QueueStats, RadioInfo, RadioLinkStats,
};
use wartui_proto::mac;
use wartui_proto::radio_link::BleCounters;

/// Generous enough for a bridge that is busy forwarding a fleet, short enough
/// that a wedged one is reported rather than waited on. The same figure the
/// notice quotes, so the two cannot disagree about how long was waited.
const REPLY_TIMEOUT: Duration = super::CONNECT_NOTICE_AFTER;

/// How long to wait for the `Fleet` report the bridge sends behind a `Status`.
const PLAN_AFTER: Duration = Duration::from_secs(1);

#[derive(ClapArgs)]
pub struct Args {
    /// The bridge, as a device path, the board's address, or the end of it (`00:08`). Detected
    /// if omitted.
    #[arg(long, value_name = "PATH|MAC")]
    pub(crate) bridge: Option<String>,

    #[command(flatten)]
    link_flag: crate::config::LinkFlag,

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
        value_parser = crate::sim_nodes(),
        conflicts_with = "link"
    )]
    sim: Option<u8>,
}

pub async fn run(args: Args) -> Result<()> {
    let (mut link, over) = super::open_remembered(
        args.bridge.as_deref(),
        args.sim,
        args.link_flag.link,
        args.config.as_deref(),
    )?;

    // The bridge announces itself on connect, so waiting for that keeps a status
    // request from going into a port nobody is listening on. The timeout is itself an
    // answer: it names the port and what to do about each cause.
    let Some(info) = wait_for_ready(&mut link, over, super::connect_wait(over)).await? else {
        bail!("{}", super::no_bridge_notice(args.bridge.as_deref(), over).await)
    };
    println!("bridge     {}, firmware {}", mac::full(&info.mac), info.fw_version);
    println!("board      {}", board_line(info.node_board));
    println!("{}", super::last_reset_line(&info));
    // A split bridge's radio restarts on its own, and this is where its reason shows.
    if let Some(radio) = &info.radio {
        println!("{}", radio_line(radio));
    }

    link.send_bulk(HostToBridge::GetStatus).context("queueing the status request")?;

    let (status, plan) = tokio::time::timeout(REPLY_TIMEOUT, wait_for_status_and_plan(&mut link))
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
        scan,
        ble,
        present,
        // The plan below names each share's slot; the members' MACs are the engine's.
        members: _,
        bluetooth,
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
    println!("scan       {}", scan_text(scan));
    println!("bluetooth  {}", ble_text(info.node_board, ble));
    println!("host       {}", host_text(&bluetooth));
    println!("pairing    {}", pairing_text(&bluetooth));
    println!("phones     {}", wardrive_esp_text(&bluetooth));
    println!("slots      {}", slots_text(info.node_board, present));
    for line in plan_lines(info.node_board, plan.as_ref()) {
        println!("{line}");
    }
    for line in diagnostic_lines(&queues, &radio_link, host_uart_errors) {
        println!("{line}");
    }
    println!("{}", counters_line(&bluetooth.counters));

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

/// What the bridge's planner is asked to scan.
pub(crate) const fn scan_text(scan: ScanMode) -> &'static str {
    match scan {
        ScanMode::Stopped => "stopped",
        ScanMode::Wifi => "wifi",
        ScanMode::WifiBluetooth => "wifi + bluetooth",
    }
}

/// The slot the bridge's planner gave the Bluetooth scan, by its label, or `none`.
pub(crate) fn ble_text(board: NodeBoard, ble: Option<Slot>) -> String {
    ble.map_or_else(|| "none".to_owned(), |slot| board.label(slot).to_string())
}

/// Who the bridge serves, and the Bluetooth host connected, if any: `bluetooth` when that host
/// holds the bridge, or after a `;` when USB holds it or the host is not let in.
pub(crate) fn host_text(status: &BluetoothStatus) -> String {
    let owner = status.owner.name();
    match (status.owner, &status.central) {
        (_, None) => owner.to_owned(),
        (HostOwner::Bluetooth, Some(central)) => format!("{owner}, {}", central_text(central)),
        (_, Some(central)) => {
            format!("{owner}; a Bluetooth host is connected, {}", central_text(central))
        }
    }
}

/// Whether a connected Bluetooth host may be served: the bridge serves only one that is both.
const fn trust_text(central: &Central) -> &'static str {
    match (central.encrypted, central.bonded) {
        (true, true) => "encrypted and bonded",
        (true, false) => "encrypted, not bonded",
        (false, true) => "bonded, not encrypted",
        (false, false) => "not encrypted or bonded",
    }
}

/// A connected Bluetooth host: whether it may be served, and its link.
fn central_text(central: &Central) -> String {
    let mut text = format!("{}, MTU {}", trust_text(central), central.mtu);
    match central.phy {
        0 => {}
        1 => text.push_str(", 1M PHY"),
        2 => text.push_str(", 2M PHY"),
        3 => text.push_str(", Coded PHY"),
        other => text.push_str(&format!(", PHY {other}")),
    }
    if central.interval_us > 0 {
        text.push_str(&format!(", {} ms interval", millis(central.interval_us)));
    }
    text
}

/// Microseconds as milliseconds, with a decimal only when it carries one: `7.5`, `30`.
fn millis(us: u32) -> String {
    let text = format!("{:.2}", f64::from(us) / 1000.0);
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// The pairing window, then the bonds the bridge keeps.
pub(crate) fn pairing_text(status: &BluetoothStatus) -> String {
    let window = match status.window {
        PairWindow::Closed => "closed".to_owned(),
        PairWindow::Open { secs } => format!("open for {secs} s more"),
        PairWindow::UntilBonded => "open until a host pairs".to_owned(),
    };
    format!("{window}, {}", bonds_text(status.bonds))
}

/// The value of the `phones` line: whether the bridge lets phones link to it as a WardriveESP
/// board.
pub(crate) const fn wardrive_esp_text(status: &BluetoothStatus) -> &'static str {
    if status.wardrive_esp { "wardrive esp on" } else { "wardrive esp off" }
}

/// The bonds the bridge keeps, counted.
pub(crate) fn bonds_text(bonds: u8) -> String {
    match bonds {
        0 => "no bond stored".to_owned(),
        1 => "1 bond stored".to_owned(),
        n => format!("{n} bonds stored"),
    }
}

/// The C6's Bluetooth host counters, since it booted.
fn counters_line(c: &BleCounters) -> String {
    let last = match c.last_disconnect {
        0 => "none".to_owned(),
        code => format!("0x{code:02x}"),
    };
    format!(
        "  bluetooth host  pairings {}  failed_pairings {}  unbonded_kicks {}  \
         last_disconnect {last}  resynced_bytes {}  dropped_writes {}",
        c.pairings, c.failed_pairings, c.unbonded_kicks, c.resynced_bytes, c.dropped_writes
    )
}

/// The bridge's plan, a line per share by `board`'s slot labels, then the channels of the pool
/// no node can take. `shares     none` while no node holds one, and `not reported` with no report.
pub(crate) fn plan_lines(board: NodeBoard, plan: Option<&Report>) -> Vec<String> {
    let Some(plan) = plan else { return vec!["shares     not reported".to_owned()] };
    if plan.shares.is_empty() {
        return vec!["shares     none".to_owned()];
    }
    let mut lines: Vec<String> = plan
        .shares
        .iter()
        .map(|share| {
            let held = if share.ble {
                "bluetooth".to_owned()
            } else {
                let count = share.channels.len();
                let unit = if count == 1 { "channel" } else { "channels" };
                format!("{count} {unit}  {}", crate::text::channel_list(share.channels))
            };
            format!("share      {:<4} {held}", board.label(share.slot).to_string())
        })
        .collect();
    if !plan.unreachable.is_empty() {
        lines.push(format!(
            "unswept    {}, which no node can take",
            crate::text::channel_list(plan.unreachable)
        ));
    }
    lines
}

/// The slots `present` names, as `board` labels them, or `none`.
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

/// The bridge's announcement over `over` within `within`, or why the link went down first.
/// `None` when the wait runs out with no failure to name ([`ReadyWait`]).
pub(crate) async fn wait_for_ready(
    link: &mut LinkHandle,
    over: crate::config::Link,
    within: Duration,
) -> Result<Option<BridgeInfo>> {
    match wait_for_ready_or_why(link, over, within).await.ready {
        Ok(info) => Ok(info),
        Err(reason) => bail!("{reason}"),
    }
}

/// What [`wait_for_ready_or_why`] came to.
#[derive(Debug)]
pub(crate) struct ReadyWaited {
    /// The announcement, `None` when the wait ran out with no failure to name, or why the link
    /// went down first.
    pub(crate) ready: Result<Option<BridgeInfo>, String>,
    /// A frame arrived that did not decode.
    pub(crate) garbled: bool,
}

/// [`wait_for_ready`], keeping the reason as text and noting whether a frame arrived that did not
/// decode: a bridge running firmware from another tree.
pub(crate) async fn wait_for_ready_or_why(
    link: &mut LinkHandle,
    over: crate::config::Link,
    within: Duration,
) -> ReadyWaited {
    let mut until = tokio::time::Instant::now() + within;
    let mut wait = ReadyWait::new(over);
    let mut garbled = false;
    loop {
        let Ok(event) = tokio::time::timeout_at(until, link.recv()).await else {
            let ready = wait.timed_out().map_or(Ok(None), Err);
            return ReadyWaited { ready, garbled };
        };
        garbled |= matches!(event, Some(LinkEvent::Garbled(_)));
        match wait.take(event) {
            Taken::Waiting => {}
            Taken::Pairing(notice) => {
                eprintln!("{notice}");
                until = pairing_deadline(until, tokio::time::Instant::now());
            }
            Taken::Ended(ready) => return ReadyWaited { ready: ready.map(Some), garbled },
        }
    }
}

/// The wait's deadline once the transport, at `now`, waits on the OS's pairing dialog: `until`,
/// or when that is sooner, the transport's own deadline for the subscription and the usual wait
/// for a `Ready` behind it. The notice comes a few seconds into the subscription, so its
/// deadline alone leaves the bridge little time to announce itself.
fn pairing_deadline(
    until: tokio::time::Instant,
    now: tokio::time::Instant,
) -> tokio::time::Instant {
    until.max(now + wartui_bridge::ble::PAIR_WITHIN + crate::CONNECT_NOTICE_AFTER)
}

/// Why a wait that announced the OS's pairing dialog ran out, when no failure is retried.
const PAIRING_RAN_OUT: &str =
    "no answer after waiting on the OS's pairing dialog; run the command again";

/// A wait for the bridge's announcement, fed one link event at a time.
///
/// Over USB the first disconnect ends it. Over Bluetooth the transport reports every failed try
/// as a disconnect and tries again a second later, so a [`DisconnectCause::Other`] is kept and
/// the wait goes on. The bridge's refusal of this PC and a USB takeover still end it at once. The
/// first [`DisconnectCause::AwaitingPairing`] is the operator's notice, and the wait stretches to
/// the transport's pairing window; a later one returns no notice. A
/// [`DisconnectCause::AwaitingReady`] says the subscription is through and the wait goes on, for
/// the bridge's `Ready`. A wait that runs out after the notice names the pairing wait, unless a
/// failure is retried or the subscription went through since.
#[derive(Debug)]
struct ReadyWait {
    over: crate::config::Link,
    /// The last failure the transport is retrying.
    retried: Option<String>,
    /// Whether the transport's wait on the OS's pairing has been announced.
    pairing: bool,
    /// Whether the subscription went through since the pairing wait last began.
    through: bool,
}

/// What one link event does to a [`ReadyWait`].
#[derive(Debug, PartialEq, Eq)]
enum Taken {
    /// The wait goes on.
    Waiting,
    /// The transport waits on the OS's pairing dialog: this notice says so, and the wait goes on.
    Pairing(String),
    /// The wait is over, with the announcement or the reason.
    Ended(Result<BridgeInfo, String>),
}

impl ReadyWait {
    const fn new(over: crate::config::Link) -> Self {
        Self { over, retried: None, pairing: false, through: false }
    }

    /// What `event` does to the wait.
    fn take(&mut self, event: Option<LinkEvent>) -> Taken {
        match event {
            Some(LinkEvent::Connected(info)) => Taken::Ended(Ok(info)),
            Some(LinkEvent::Disconnected { reason, cause: DisconnectCause::AwaitingPairing }) => {
                self.through = false;
                if std::mem::replace(&mut self.pairing, true) {
                    Taken::Waiting
                } else {
                    Taken::Pairing(reason)
                }
            }
            Some(LinkEvent::Disconnected { reason, cause: DisconnectCause::Other })
                if self.over == crate::config::Link::Bluetooth =>
            {
                self.retried = Some(reason);
                Taken::Waiting
            }
            Some(LinkEvent::Disconnected { cause: DisconnectCause::AwaitingReady, .. }) => {
                // The failures before it are behind the subscription that went through.
                self.through = true;
                self.retried = None;
                Taken::Waiting
            }
            Some(LinkEvent::Disconnected { reason, .. }) => Taken::Ended(Err(reason)),
            Some(_) => Taken::Waiting,
            None => Taken::Ended(Err("the link closed".to_owned())),
        }
    }

    /// Why the wait ran out: the last failure the transport was retrying, else the pairing wait
    /// once it was announced and while the subscription has not gone through, if either.
    fn timed_out(self) -> Option<String> {
        let pairing = self.pairing && !self.through;
        self.retried.or_else(|| pairing.then(|| PAIRING_RAN_OUT.to_owned()))
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
            Awaited::Fleet(_) | Awaited::Other => {}
        }
    }
}

/// [`wait_for_status`], then the `Fleet` report the bridge sends behind it, or the last one
/// before it when none follows within [`PLAN_AFTER`].
pub(crate) async fn wait_for_status_and_plan(
    link: &mut wartui_bridge::LinkHandle,
) -> Result<(BridgeToHost, Option<Report>)> {
    let mut plan = None;
    let status = loop {
        match awaited(link.recv().await)? {
            Awaited::Status(status) => break *status,
            Awaited::Fleet(report) => plan = Some(*report),
            Awaited::Fault(line) => println!("{line}"),
            Awaited::Other => {}
        }
    };
    let deadline = tokio::time::Instant::now() + PLAN_AFTER;
    loop {
        let Ok(event) = tokio::time::timeout_at(deadline, link.recv()).await else {
            return Ok((status, plan));
        };
        match awaited(event)? {
            Awaited::Fleet(report) => return Ok((status, Some(*report))),
            Awaited::Fault(line) => println!("{line}"),
            Awaited::Status(_) | Awaited::Other => {}
        }
    }
}

/// What one link event means while waiting for the `Status` answer.
#[derive(Debug, PartialEq)]
enum Awaited {
    /// The answer.
    Status(Box<BridgeToHost>),
    /// The planner's report.
    Fleet(Box<Report>),
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
        Some(LinkEvent::Message(BridgeToHost::Fleet(report))) => {
            Ok(Awaited::Fleet(Box::new(report)))
        }
        Some(LinkEvent::Disconnected { reason, .. }) => bail!("{reason}"),
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
        Awaited, ReadyWait, Report, ScanMode, Taken, awaited, ble_text, board_line, counters_line,
        diagnostic_lines, fan_text, host_text, human_uptime, pairing_deadline, pairing_text,
        plan_lines, radio_line, scan_text, silence_note, slots_text, wardrive_esp_text,
    };
    use std::time::Duration;

    use crate::config::Link;
    use wartui_bridge::{DisconnectCause, LinkEvent};
    use wartui_proto::ble::ATT_MTU;
    use wartui_proto::bus::{NodeBoard, Slot};
    use wartui_proto::fan::{FanDuty, FanStats};
    use wartui_proto::fleet::Share;
    use wartui_proto::link::{BluetoothStatus, Central, HostOwner, PairWindow};
    use wartui_proto::link::{
        BridgeToHost, LoopPhase, QueueId, QueueStat, QueueStats, RadioInfo, RadioLinkStats,
    };
    use wartui_proto::plan::{ChannelPool, ChannelSet, IndexRun};
    use wartui_proto::radio_link::BleCounters;
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
    fn status_prints_scan_mode_when_bridge_reports_one() {
        assert_eq!(scan_text(ScanMode::Stopped), "stopped");
        assert_eq!(scan_text(ScanMode::Wifi), "wifi");
        assert_eq!(scan_text(ScanMode::WifiBluetooth), "wifi + bluetooth");
    }

    /// A plan holding `shares`, scanning Wi-Fi and Bluetooth over the `all` pool.
    fn plan(shares: &[Share], unreachable: ChannelSet) -> Report {
        Report {
            mode: ScanMode::WifiBluetooth,
            pool: ChannelPool::All,
            shares: shares.iter().copied().collect(),
            unreachable,
        }
    }

    #[test]
    fn status_prints_one_line_per_share_when_bridge_reports_a_plan() {
        let a1 = Slot::new(0, 0).expect("A1");
        let b2 = Slot::new(1, 1).expect("B2");
        let channels = ChannelSet::from_run(IndexRun::new(0, 10));
        let shares = [
            Share { slot: a1, epoch: 3, ble: false, channels },
            Share { slot: b2, epoch: 4, ble: true, channels: ChannelSet::empty() },
        ];
        assert_eq!(
            plan_lines(NodeBoard::Carrier14, Some(&plan(&shares, ChannelSet::empty()))),
            ["share      N1   11 channels  1-11", "share      N9   bluetooth"]
        );
    }

    #[test]
    fn status_prints_unswept_channels_when_no_node_can_take_them() {
        let a1 = Slot::new(0, 0).expect("A1");
        let channels = ChannelSet::from_run(IndexRun::new(0, 0));
        let shares = [Share { slot: a1, epoch: 3, ble: false, channels }];
        let unreachable = ChannelSet::from_run(IndexRun::new(14, 38));
        let lines = plan_lines(NodeBoard::Carrier14, Some(&plan(&shares, unreachable)));
        assert_eq!(lines[0], "share      N1   1 channel  1");
        assert_eq!(lines[1], "unswept    36-165, which no node can take");
    }

    #[test]
    fn status_prints_no_shares_when_plan_is_empty_or_unreported() {
        let stopped = Report { mode: ScanMode::Stopped, ..plan(&[], ChannelSet::empty()) };
        assert_eq!(plan_lines(NodeBoard::Carrier14, Some(&stopped)), ["shares     none"]);
        assert_eq!(plan_lines(NodeBoard::Carrier14, None), ["shares     not reported"]);
    }

    #[test]
    fn status_cmd_keeps_plan_when_fleet_report_arrives() {
        let report = plan(&[], ChannelSet::empty());
        let event = LinkEvent::Message(BridgeToHost::Fleet(report.clone()));
        assert_eq!(awaited(Some(event)).expect("not an error"), Awaited::Fleet(Box::new(report)));
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
            scan: ScanMode::Stopped,
            ble: None,
            present: 0,
            members: Default::default(),
            bluetooth: Default::default(),
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

    /// A bonded host on an encrypted 2M link at a 30 ms interval.
    const CENTRAL: Central =
        Central { encrypted: true, bonded: true, mtu: ATT_MTU, phy: 2, interval_us: 30_000 };

    #[test]
    fn status_prints_host_as_owner_alone_when_no_bluetooth_host_is_connected() {
        for (owner, text) in [
            (HostOwner::None, "none"),
            (HostOwner::Usb, "usb"),
            (HostOwner::Bluetooth, "bluetooth"),
        ] {
            let status = BluetoothStatus { owner, ..BluetoothStatus::default() };
            assert_eq!(host_text(&status), text);
        }
    }

    #[test]
    fn status_prints_bluetooth_host_link_when_it_holds_the_bridge() {
        let status = BluetoothStatus {
            owner: HostOwner::Bluetooth,
            central: Some(CENTRAL),
            ..BluetoothStatus::default()
        };
        assert_eq!(
            host_text(&status),
            "bluetooth, encrypted and bonded, MTU 182, 2M PHY, 30 ms interval"
        );
    }

    #[test]
    fn status_prints_waiting_bluetooth_host_when_usb_holds_the_bridge() {
        let central = Central { bonded: false, phy: 0, interval_us: 7_500, ..CENTRAL };
        let status = BluetoothStatus {
            owner: HostOwner::Usb,
            central: Some(central),
            ..BluetoothStatus::default()
        };
        assert_eq!(
            host_text(&status),
            "usb; a Bluetooth host is connected, encrypted, not bonded, MTU 182, 7.5 ms interval"
        );
    }

    #[test]
    fn status_prints_pairing_window_and_bonds_when_bridge_reports_them() {
        let pairing = |window, bonds| {
            pairing_text(&BluetoothStatus { window, bonds, ..BluetoothStatus::default() })
        };
        assert_eq!(pairing(PairWindow::Closed, 2), "closed, 2 bonds stored");
        assert_eq!(pairing(PairWindow::Open { secs: 87 }, 1), "open for 87 s more, 1 bond stored");
        assert_eq!(pairing(PairWindow::UntilBonded, 0), "open until a host pairs, no bond stored");
    }

    #[test]
    fn status_prints_wardrive_esp_setting_when_bridge_reports_it() {
        let text = |wardrive_esp| {
            wardrive_esp_text(&BluetoothStatus { wardrive_esp, ..BluetoothStatus::default() })
        };
        assert_eq!(text(true), "wardrive esp on");
        assert_eq!(text(false), "wardrive esp off");
    }

    #[test]
    fn status_prints_bluetooth_host_counters_when_bridge_reports_them() {
        let counters = BleCounters {
            pairings: 2,
            failed_pairings: 1,
            unbonded_kicks: 3,
            last_disconnect: 0x13,
            resynced_bytes: 40,
            dropped_writes: 0,
            ..BleCounters::default()
        };
        assert_eq!(
            counters_line(&counters),
            "  bluetooth host  pairings 2  failed_pairings 1  unbonded_kicks 3  \
             last_disconnect 0x13  resynced_bytes 40  dropped_writes 0"
        );
        assert!(counters_line(&BleCounters::default()).contains("last_disconnect none"));
    }

    #[test]
    fn status_cmd_fails_when_link_closes_before_answer() {
        assert!(awaited(None).is_err());
    }

    fn down(reason: &str, cause: DisconnectCause) -> Option<LinkEvent> {
        Some(LinkEvent::Disconnected { reason: reason.to_owned(), cause })
    }

    #[test]
    fn ready_wait_ends_on_first_disconnect_when_over_usb() {
        let mut wait = ReadyWait::new(Link::Usb);
        let ended = wait.take(down("could not open", DisconnectCause::Other));
        assert_eq!(ended, Taken::Ended(Err("could not open".to_owned())));
    }

    #[test]
    fn ready_wait_keeps_waiting_and_names_last_failure_when_bluetooth_retries() {
        let mut wait = ReadyWait::new(Link::Bluetooth);
        assert_eq!(wait.take(down("timed out", DisconnectCause::Other)), Taken::Waiting);
        assert_eq!(wait.take(down("not found", DisconnectCause::Other)), Taken::Waiting);
        assert_eq!(wait.timed_out(), Some("not found".to_owned()));
    }

    #[test]
    fn ready_wait_ends_at_once_when_bridge_refuses_or_usb_takes_it_over_bluetooth() {
        for cause in [
            DisconnectCause::NotBonded,
            DisconnectCause::MaybeNotBonded,
            DisconnectCause::UsbTookBridge,
        ] {
            let mut wait = ReadyWait::new(Link::Bluetooth);
            assert_eq!(wait.take(down("refused", cause)), Taken::Ended(Err("refused".to_owned())));
        }
    }

    #[test]
    fn ready_wait_returns_pairing_notice_once_when_transport_awaits_pairing() {
        let mut wait = ReadyWait::new(Link::Bluetooth);
        let notice = "pairing: click Connect if macOS shows a dialog for 44:C0";
        let pairing = DisconnectCause::AwaitingPairing;
        assert_eq!(wait.take(down(notice, pairing)), Taken::Pairing(notice.to_owned()));
        assert_eq!(wait.take(down(notice, pairing)), Taken::Waiting, "said once");
    }

    #[test]
    fn ready_wait_names_pairing_wait_when_it_runs_out_after_notice() {
        let mut wait = ReadyWait::new(Link::Bluetooth);
        let pairing = DisconnectCause::AwaitingPairing;
        assert!(matches!(wait.take(down("pairing", pairing)), Taken::Pairing(_)));
        assert_eq!(
            wait.timed_out().as_deref(),
            Some("no answer after waiting on the OS's pairing dialog; run the command again")
        );
    }

    #[test]
    fn ready_wait_names_retried_failure_when_it_runs_out_after_notice() {
        let mut wait = ReadyWait::new(Link::Bluetooth);
        assert_eq!(wait.take(down("timed out", DisconnectCause::Other)), Taken::Waiting);
        let pairing = DisconnectCause::AwaitingPairing;
        assert!(matches!(wait.take(down("pairing", pairing)), Taken::Pairing(_)));
        assert_eq!(wait.timed_out(), Some("timed out".to_owned()));
    }

    #[test]
    fn ready_wait_keeps_waiting_when_subscription_is_through() {
        // Without its own arm, the first pairing of a one-shot command would fail at the click.
        let mut wait = ReadyWait::new(Link::Bluetooth);
        let pairing = DisconnectCause::AwaitingPairing;
        assert!(matches!(wait.take(down("pairing", pairing)), Taken::Pairing(_)));
        let ready = "waiting for 44:C0 to answer; a USB host may hold the bridge";
        assert_eq!(wait.take(down(ready, DisconnectCause::AwaitingReady)), Taken::Waiting);
    }

    #[test]
    fn ready_wait_names_nothing_when_it_runs_out_after_subscription_is_through() {
        // The caller falls through to its usual no-bridge notice.
        let mut wait = ReadyWait::new(Link::Bluetooth);
        assert_eq!(wait.take(down("timed out", DisconnectCause::Other)), Taken::Waiting);
        let pairing = DisconnectCause::AwaitingPairing;
        assert!(matches!(wait.take(down("pairing", pairing)), Taken::Pairing(_)));
        let ready = "waiting for 44:C0 to answer; a USB host may hold the bridge";
        assert_eq!(wait.take(down(ready, DisconnectCause::AwaitingReady)), Taken::Waiting);
        assert_eq!(wait.timed_out(), None);
    }

    #[test]
    fn ready_wait_names_nothing_when_it_runs_out_with_no_failure() {
        let mut wait = ReadyWait::new(Link::Bluetooth);
        assert_eq!(
            wait.take(Some(LinkEvent::Garbled(wartui_proto::link::LinkError::Corrupt))),
            Taken::Waiting
        );
        assert_eq!(wait.timed_out(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn ready_wait_stretches_to_pairing_window_when_notice_comes_before_deadline() {
        // A one-shot command's 17 s wait, and the transport's notice 3 s in.
        let start = tokio::time::Instant::now();
        let until = start + Duration::from_secs(17);
        let now = start + Duration::from_secs(3);
        let stretched = pairing_deadline(until, now);
        // The subscription's deadline, and room for the `Ready` behind it.
        assert_eq!(stretched, now + wartui_bridge::ble::PAIR_WITHIN + crate::CONNECT_NOTICE_AFTER);
        assert!(stretched > until);
    }

    #[tokio::test(start_paused = true)]
    async fn ready_wait_keeps_deadline_when_it_is_past_pairing_window() {
        let now = tokio::time::Instant::now();
        let window = wartui_bridge::ble::PAIR_WITHIN + crate::CONNECT_NOTICE_AFTER;
        let until = now + window + Duration::from_secs(1);
        assert_eq!(pairing_deadline(until, now), until);
    }
}
