//! `wartui scan` — start or stop the bridge's scan, or show what it is doing.
//!
//! The bridge plans the fleet and keeps scanning with no host attached, so this is how a board is
//! set scanning for a drive with nothing on the other end of its cable. Wi-Fi is the scan, and
//! `--bluetooth` adds Bluetooth to it: the bridge picks the node, and only once two nodes can
//! scan, since the one it gives Bluetooth sniffs no Wi-Fi. The bridge boots stopped and keeps no
//! mode across a reboot.
//!
//! Each action waits for the bridge to report the change, and fails when it does not within
//! [`REPLY_TIMEOUT`]. `start` then reads the bridge's reports for [`SETTLE`] before it prints the
//! plan, since a node joins the plan on its next heartbeat. A fleet still short of two nodes gets
//! no Bluetooth node, which is said and is not a failure: the bridge gives Bluetooth to a node once
//! a second joins. Without an action, nothing is sent but a status request.
//!
//! The bridge is resolved to one board as `wartui pool` resolves it. A `wartui run` holds the
//! port, so this runs only while none is attached; in `run`, `b` toggles Bluetooth and quitting
//! stops the scan.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args as ClapArgs, Subcommand};
use wartui_bridge::{LinkEvent, LinkHandle};
use wartui_core::runtime::stop_scan;
use wartui_proto::bus::{NodeBoard, Slot};
use wartui_proto::fleet::{Report, ScanMode};
use wartui_proto::link::{BridgeToHost, HostToBridge};
use wartui_proto::mac;

use crate::status::{ble_text, plan_lines, scan_text};

/// How long the bridge has to report a change, or to answer the status request.
const REPLY_TIMEOUT: Duration = super::CONNECT_NOTICE_AFTER;

/// How long `start` reads reports once the bridge shows the mode: an idle node heartbeats every
/// second, so every node present has joined the plan by then.
const SETTLE: Duration = Duration::from_secs(3);

#[derive(ClapArgs)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Args {
    #[command(subcommand)]
    action: Option<Action>,

    #[command(flatten)]
    target: Target,
}

#[derive(Subcommand)]
enum Action {
    /// Start the scan: Wi-Fi, and Bluetooth too with `--bluetooth`. The bridge scans until
    /// stopped, with or without a host.
    Start {
        /// Add Bluetooth to the scan. The bridge gives it to one node once two can scan.
        #[arg(long)]
        bluetooth: bool,

        #[command(flatten)]
        target: Target,
    },
    /// Stop the scan: every node idles until the scan starts again.
    Stop {
        #[command(flatten)]
        target: Target,
    },
}

/// Which bridge, shared by the bare command and each action.
#[derive(ClapArgs)]
struct Target {
    /// The bridge, as a device path, the board's address, or the end of it (`00:08`). Detected
    /// if omitted.
    #[arg(long, value_name = "PATH|MAC")]
    bridge: Option<String>,

    /// Read the remembered bridge from this file instead of the default `wartui.toml`. A file
    /// that does not load is no bridge remembered. Never written; see
    /// `crates/wartui/README.md` § "Config file".
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Ask the simulator instead of hardware. It boots stopped, as a bridge does.
    #[arg(
        long,
        value_name = "NODES",
        num_args = 0..=1,
        default_missing_value = "3",
        value_parser = crate::sim_nodes()
    )]
    sim: Option<u8>,
}

/// What to ask the bridge for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wanted {
    /// Nothing: show the scan.
    Show,
    /// Start, with Bluetooth when `true`.
    Start(bool),
    /// Stop.
    Stop,
}

pub async fn run(args: Args) -> Result<()> {
    let (wanted, target) = match args.action {
        None => (Wanted::Show, args.target),
        Some(Action::Start { bluetooth, target }) => (Wanted::Start(bluetooth), target),
        Some(Action::Stop { target }) => (Wanted::Stop, target),
    };
    let (mut link, info) = connect(&target).await?;
    let bridge = mac::full(&info.mac);
    match wanted {
        Wanted::Show => {}
        Wanted::Start(false) => println!("starting the wifi scan on bridge {bridge}"),
        Wanted::Start(true) => println!("starting the wifi and bluetooth scan on bridge {bridge}"),
        Wanted::Stop => println!("stopping the scan on bridge {bridge}"),
    }
    for line in act(&mut link, info.node_board, wanted).await? {
        println!("{line}");
    }
    Ok(())
}

/// The bridge `target` names, connected and announced.
async fn connect(target: &Target) -> Result<(LinkHandle, wartui_bridge::BridgeInfo)> {
    if target.sim.is_none() {
        return super::connect_one(target.bridge.as_deref(), target.config.as_deref()).await;
    }
    let mut link = super::open(None, target.sim, super::memory(None))?;
    let info = tokio::time::timeout(REPLY_TIMEOUT, crate::status::wait_for_ready(&mut link))
        .await
        .context("the simulator did not announce itself")??;
    Ok((link, info))
}

/// Carry out `wanted` on the bridge behind `link`, whose nodes `board` carries, and return the
/// lines that report the outcome. Fails when the bridge does not show the change in time.
async fn act(link: &mut LinkHandle, board: NodeBoard, wanted: Wanted) -> Result<Vec<String>> {
    match wanted {
        Wanted::Show => show(link, board).await,
        Wanted::Start(bluetooth) => start(link, board, bluetooth).await,
        Wanted::Stop => {
            if !stop_scan(link, REPLY_TIMEOUT).await {
                bail!("the bridge did not report the scan stopped");
            }
            Ok(vec![format!("scan       {}", scan_text(ScanMode::Stopped))])
        }
    }
}

/// The mode, the Bluetooth node and every share, from one status request.
async fn show(link: &mut LinkHandle, board: NodeBoard) -> Result<Vec<String>> {
    link.send_bulk(HostToBridge::GetStatus).context("queueing the status request")?;
    let (status, plan) =
        tokio::time::timeout(REPLY_TIMEOUT, crate::status::wait_for_status_and_plan(link))
            .await
            .context("the bridge did not answer a status request")??;
    let BridgeToHost::Status { scan, ble, pool, .. } = status else {
        unreachable!("wait_for_status_and_plan only returns Status")
    };
    let mut lines = vec![
        format!("scan       {}", scan_text(scan)),
        format!("bluetooth  {}", pick_text(board, scan, ble)),
        format!("pool       {pool}"),
    ];
    lines.extend(plan_lines(board, plan.as_ref()));
    Ok(lines)
}

/// Start the scan, wait for the bridge to report it, then read its plan as nodes join.
async fn start(link: &mut LinkHandle, board: NodeBoard, bluetooth: bool) -> Result<Vec<String>> {
    link.send_urgent(HostToBridge::StartScan { bluetooth }).context("queueing the start")?;
    link.send_bulk(HostToBridge::GetStatus).context("queueing the status request")?;
    let mode = if bluetooth { ScanMode::WifiBluetooth } else { ScanMode::Wifi };
    let mut seen = Seen::default();
    if !watch(link, REPLY_TIMEOUT, &mut seen, |seen| seen.mode == Some(mode)).await? {
        bail!("the bridge did not report the scan started");
    }
    watch(link, SETTLE, &mut seen, |_| false).await?;
    let mut lines = vec![format!("scan       {}", scan_text(mode))];
    if bluetooth {
        lines.push(format!("bluetooth  {}", pick_text(board, mode, seen.pick)));
    }
    lines.extend(plan_lines(board, seen.plan.as_ref()));
    Ok(lines)
}

/// The node the bridge gave Bluetooth, or why it gave none.
fn pick_text(board: NodeBoard, mode: ScanMode, pick: Option<Slot>) -> String {
    match (mode, pick) {
        (_, Some(_)) => ble_text(board, pick),
        (ScanMode::WifiBluetooth, None) => {
            "none yet: it needs two nodes that can scan, and a lone node stays on wifi".to_owned()
        }
        (ScanMode::Stopped | ScanMode::Wifi, None) => "none".to_owned(),
    }
}

/// What the bridge has reported so far.
#[derive(Debug, Default)]
struct Seen {
    /// Its mode, from a `Status` or a `Fleet` report.
    mode: Option<ScanMode>,
    /// The node it gave Bluetooth, from the same.
    pick: Option<Slot>,
    /// Its last `Fleet` report.
    plan: Option<Report>,
}

impl Seen {
    /// Take in one link event.
    fn note(&mut self, event: &BridgeToHost) {
        match event {
            BridgeToHost::Status { scan, ble, .. } => {
                self.mode = Some(*scan);
                self.pick = *ble;
            }
            BridgeToHost::Fleet(report) => {
                self.mode = Some(report.mode);
                self.pick = report.shares.iter().find(|share| share.ble).map(|share| share.slot);
                self.plan = Some(report.clone());
            }
            _ => {}
        }
    }
}

/// Read the link for up to `wait`, noting each report in `seen`, until `done` holds. Prints each
/// fault the bridge reports meanwhile. `Ok(false)` when the wait ran out first.
async fn watch(
    link: &mut LinkHandle,
    wait: Duration,
    seen: &mut Seen,
    done: impl Fn(&Seen) -> bool,
) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + wait;
    while !done(seen) {
        let Ok(event) = tokio::time::timeout_at(deadline, link.recv()).await else {
            return Ok(false);
        };
        match event {
            Some(LinkEvent::Message(BridgeToHost::Error { message })) => {
                println!("fault      {message}");
            }
            Some(LinkEvent::Message(message)) => seen.note(&message),
            Some(LinkEvent::Disconnected { reason }) => bail!("{reason}"),
            Some(_) => {}
            None => bail!("the link closed"),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use wartui_bridge::sim::{SimConfig, SimTransport};
    use wartui_proto::bus::{NodeBoard, Slot};
    use wartui_proto::fleet::ScanMode;

    use super::{LinkHandle, Wanted, act, pick_text};

    /// A simulated bridge of `nodes` nodes, connected, and the board it announced.
    async fn sim(nodes: u8) -> (LinkHandle, NodeBoard) {
        let config = SimConfig { node_count: nodes, speed: 20.0, ..SimConfig::default() };
        let mut link = SimTransport::new(config).start().expect("starting the simulator");
        let info = crate::status::wait_for_ready(&mut link).await.expect("the simulator connects");
        (link, info.node_board)
    }

    #[tokio::test(start_paused = true)]
    async fn scan_cmd_reports_wifi_and_shares_when_started() {
        let (mut link, board) = sim(3).await;
        let lines = act(&mut link, board, Wanted::Start(false)).await.expect("the start");
        assert_eq!(lines[0], "scan       wifi", "{lines:?}");
        assert!(lines.iter().all(|line| !line.starts_with("bluetooth")), "{lines:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn scan_cmd_names_bridges_pick_when_started_with_bluetooth() {
        let (mut link, board) = sim(3).await;
        let lines = act(&mut link, board, Wanted::Start(true)).await.expect("the start");
        assert_eq!(lines[0], "scan       wifi + bluetooth", "{lines:?}");
        let pick = lines.iter().find(|line| line.starts_with("bluetooth")).expect("a pick");
        assert!(!pick.contains("none"), "{lines:?}");
        assert!(lines.iter().any(|line| line.ends_with(" bluetooth")), "its share: {lines:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn scan_cmd_says_bluetooth_needs_two_nodes_when_fleet_is_one_node() {
        let (mut link, board) = sim(1).await;
        let lines = act(&mut link, board, Wanted::Start(true)).await.expect("the start");
        assert_eq!(lines[0], "scan       wifi + bluetooth", "{lines:?}");
        let pick = lines.iter().find(|line| line.starts_with("bluetooth")).expect("a pick");
        assert!(pick.contains("needs two nodes"), "{lines:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn scan_cmd_reports_stopped_when_stopped_after_start() {
        let (mut link, board) = sim(2).await;
        act(&mut link, board, Wanted::Start(false)).await.expect("the start");
        let lines = act(&mut link, board, Wanted::Stop).await.expect("the stop");
        assert_eq!(lines, ["scan       stopped"]);
        let shown = act(&mut link, board, Wanted::Show).await.expect("the status");
        assert_eq!(shown[0], "scan       stopped", "{shown:?}");
        assert!(shown.contains(&"shares     none".to_owned()), "{shown:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn scan_cmd_shows_stopped_without_starting_when_given_no_action() {
        // The simulator boots stopped, as a bridge does, and looking leaves it so.
        let (mut link, board) = sim(3).await;
        let lines = act(&mut link, board, Wanted::Show).await.expect("the status");
        assert_eq!(lines[..2], ["scan       stopped", "bluetooth  none"], "{lines:?}");
        let again = act(&mut link, board, Wanted::Show).await.expect("the status");
        assert_eq!(again[0], "scan       stopped", "{again:?}");
    }

    #[test]
    fn scan_cmd_names_pick_by_slot_label_when_bridge_reports_one() {
        let board = NodeBoard::Carrier14;
        assert_eq!(pick_text(board, ScanMode::WifiBluetooth, Slot::new(1, 1)), "N9");
        assert_eq!(pick_text(board, ScanMode::Wifi, None), "none");
        assert!(pick_text(board, ScanMode::WifiBluetooth, None).contains("two nodes"));
    }
}
