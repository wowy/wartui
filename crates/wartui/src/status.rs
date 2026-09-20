//! `wartui status` — ask the bridge how it is doing.
//!
//! The only command that proves the link works in both directions, which makes
//! it the first thing to reach for when frames are not arriving: a bridge that
//! answers is listening, and one that does not is either wedged or not there.
//!
//! `dropped_tx` is the number worth watching: frames the bridge threw away because
//! the host was not draining the USB endpoint, so observations lost on this side of
//! the radio rather than on the air.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use wartui_bridge::LinkEvent;
use wartui_proto::link::{BridgeToHost, HostToBridge};

/// Generous enough for a bridge that is busy forwarding a fleet, short enough
/// that a wedged one is reported rather than waited on. The same figure the
/// notice quotes, so the two cannot disagree about how long was waited.
const REPLY_TIMEOUT: Duration = super::CONNECT_NOTICE_AFTER;

#[derive(ClapArgs)]
pub struct Args {
    /// The bridge, as a device path or as the board's address. Detected if omitted.
    #[arg(long, value_name = "PATH|MAC")]
    pub(crate) bridge: Option<String>,

    /// Ask the simulator instead of hardware.
    #[arg(long, value_name = "NODES", num_args = 0..=1, default_missing_value = "3")]
    sim: Option<u8>,
}

pub async fn run(args: Args) -> Result<()> {
    let mut link = super::open(args.bridge.as_deref(), args.sim, 0)?;

    // The bridge announces itself on connect, so waiting for that keeps a status
    // request from going into a port nobody is listening on. The timeout is itself an
    // answer: it names the port and what to do about each of the three causes.
    let info = match tokio::time::timeout(REPLY_TIMEOUT, wait_for_ready(&mut link)).await {
        Ok(ready) => ready?,
        Err(_) => bail!("{}", super::no_bridge_notice(args.bridge.as_deref())),
    };
    println!(
        "bridge     {} on {:?}, firmware {}",
        super::mac(&info.mac),
        info.chip,
        info.fw_version
    );
    println!("{}", super::last_reset_line(&info));
    // A board fact rather than a chip fact, and the one that decides whether `wartui
    // run` pushes anything at a screen. Said either way, so an absent line never has
    // to be interpreted.
    match info.panel {
        Some(panel) => println!("panel      {} x {} characters", panel.cols, panel.rows),
        None => println!("panel      none"),
    }

    link.send_bulk(HostToBridge::GetStatus).context("queueing the status request")?;

    let status = tokio::time::timeout(REPLY_TIMEOUT, wait_for_status(&mut link))
        .await
        .context("the bridge did not answer a status request")??;

    let BridgeToHost::Status { channel, peer_count, rx_count, dropped_tx, uptime_ms } = status
    else {
        unreachable!("wait_for_status only returns Status")
    };

    println!("channel    {channel}");
    println!("peers      {peer_count}");
    println!("received   {rx_count} frames");
    println!("dropped    {dropped_tx} frames");
    println!("uptime     {}", human_uptime(uptime_ms));
    // Read at the moment the bridge announced rather than now, which is close enough:
    // nothing wartui writes allocates, and what matters is the trend across captures.
    println!("heap free  {} bytes", info.heap_free);

    if dropped_tx > 0 {
        // Cumulative since the bridge booted, and one left powered with nothing
        // attached drops everything it hears — so a large number is not a fault.
        println!(
            "\n{dropped_tx} frames were discarded over those {}, whenever no host was \n\
             reading fast enough. That includes any time the bridge spent powered \n\
             with nothing attached. `wartui run` reports drops from the moment it \n\
             connects, which is the number that says whether a capture lost data.",
            human_uptime(uptime_ms)
        );
    }
    if rx_count == 0 {
        println!(
            "\nThe bridge has heard nothing. Check the nodes are powered and on channel {channel}."
        );
    }

    Ok(())
}

async fn wait_for_ready(link: &mut wartui_bridge::LinkHandle) -> Result<wartui_bridge::BridgeInfo> {
    loop {
        match link.recv().await {
            Some(LinkEvent::Connected(info)) => return Ok(info),
            Some(LinkEvent::Disconnected { reason }) => bail!("{reason}"),
            Some(_) => {}
            None => bail!("the link closed"),
        }
    }
}

async fn wait_for_status(link: &mut wartui_bridge::LinkHandle) -> Result<BridgeToHost> {
    loop {
        match link.recv().await {
            Some(LinkEvent::Message(status @ BridgeToHost::Status { .. })) => return Ok(status),
            Some(LinkEvent::Message(BridgeToHost::Error { message })) => {
                bail!("the bridge refused it: {message}")
            }
            Some(LinkEvent::Disconnected { reason }) => bail!("{reason}"),
            Some(_) => {}
            None => bail!("the link closed"),
        }
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
    use super::human_uptime;

    #[test]
    fn uptime_reads_in_the_largest_useful_unit() {
        assert_eq!(human_uptime(0), "0s");
        assert_eq!(human_uptime(59_999), "59s");
        assert_eq!(human_uptime(60_000), "1m 0s");
        assert_eq!(human_uptime(3_599_000), "59m 59s");
        assert_eq!(human_uptime(3_600_000), "1h 0m");
        assert_eq!(human_uptime(7_380_000), "2h 3m");
    }
}
