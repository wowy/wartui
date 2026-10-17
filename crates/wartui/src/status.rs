//! `wartui status` — ask the bridge how it is doing.
//!
//! The only command that proves the link works in both directions, which makes
//! it the first thing to reach for when frames are not arriving: a bridge that
//! answers is listening, and one that does not is either wedged or not there.
//!
//! `dropped_tx` is the number worth watching: frames the bridge threw away because
//! they arrived faster than the host drained the USB endpoint — a host not reading,
//! or a burst too big to queue — so observations lost on this side of the radio
//! rather than on the air.
//!
//! `oversize` counts frames the bridge heard and dropped for being longer than any
//! wartui node sends: somebody else's ESP-NOW traffic, harmless to the capture.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use wartui_bridge::LinkEvent;
use wartui_proto::air::CONTROL_CHANNEL;
use wartui_proto::link::{BridgeToHost, HostToBridge};
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
    #[arg(long, value_name = "NODES", num_args = 0..=1, default_missing_value = "3")]
    sim: Option<u8>,
}

pub async fn run(args: Args) -> Result<()> {
    let mut link =
        super::open(args.bridge.as_deref(), args.sim, 0, super::memory(args.config.as_deref()))?;

    // The bridge announces itself on connect, so waiting for that keeps a status
    // request from going into a port nobody is listening on. The timeout is itself an
    // answer: it names the port and what to do about each of the three causes.
    let info = match tokio::time::timeout(REPLY_TIMEOUT, wait_for_ready(&mut link)).await {
        Ok(ready) => ready?,
        Err(_) => bail!("{}", super::no_bridge_notice(args.bridge.as_deref())),
    };
    println!(
        "bridge     {} on {:?}, firmware {}",
        mac::full(&info.mac),
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

    let BridgeToHost::Status { peer_count, rx_count, dropped_tx, uptime_ms, rx_oversize } = status
    else {
        unreachable!("wait_for_status only returns Status")
    };

    println!("peers      {peer_count}");
    println!("received   {rx_count} frames");
    println!("dropped    {dropped_tx} frames");
    println!("oversize   {rx_oversize} frames");
    println!("uptime     {}", human_uptime(uptime_ms));
    // Read at the moment the bridge announced rather than now, which is close enough:
    // nothing wartui writes allocates, and what matters is the trend across captures.
    println!("heap free  {} bytes", info.heap_free);

    if dropped_tx > 0 {
        // Cumulative since the bridge booted, and one left powered with nothing
        // attached drops everything it hears — so a large number is not a fault.
        println!(
            "\n{dropped_tx} frames were discarded over those {}, because a host was \n\
             not reading or a burst arrived faster than USB could drain it. That \n\
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

/// What to say when no node's frame has reached the bridge, or `None` when one has.
///
/// Oversize frames are ESP-NOW from somebody else, so a bridge that has counted only those
/// has a working radio and no fleet to hear.
fn silence_note(rx_count: u32, rx_oversize: u32) -> Option<String> {
    if rx_count > 0 {
        return None;
    }
    let heard = if rx_oversize > 0 {
        "The radio is hearing ESP-NOW traffic, but none of it is from a wartui node."
    } else {
        "The bridge has heard no wartui frames."
    };
    Some(format!("{heard} Check the nodes are powered and on channel {CONTROL_CHANNEL}."))
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
    use super::{human_uptime, silence_note};

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
    fn status_cmd_names_foreign_traffic_when_bridge_heard_only_oversize_frames() {
        let note = silence_note(0, 3).expect("no node was heard");
        assert!(note.starts_with("The radio is hearing ESP-NOW traffic, but none"), "{note}");
        assert!(note.contains("powered and on channel"), "{note}");
    }

    #[test]
    fn status_cmd_says_heard_no_wartui_frames_when_bridge_counted_none() {
        let note = silence_note(0, 0).expect("no node was heard");
        assert!(note.starts_with("The bridge has heard no wartui frames."), "{note}");
    }

    #[test]
    fn status_cmd_omits_silence_note_when_a_node_frame_arrived() {
        assert_eq!(silence_note(1, 3), None);
    }
}
