//! `wartui fan` — read or set the carrier's lid fan duty.
//!
//! The bridge keeps the duty on its own flash and boots into it, so it holds across the bridge's
//! reboots and with no host attached. `wartui.toml` holds no copy. The bridge is resolved to one
//! board as `wartui power-cycle` resolves it. A bench bridge has no fan, and is refused before
//! anything is sent.
//!
//! With a duty, the bridge applies it at once and the command waits two of its tach windows, so
//! the speed it prints is read at the new duty.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use wartui_bridge::remember::BridgeMemory;
use wartui_bridge::serial::{discover_bridges, probe_all};
use wartui_bridge::{LinkEvent, LinkHandle};
use wartui_proto::bus::NodeBoard;
use wartui_proto::fan::{FanDuty, TACH_WINDOW_MS};
use wartui_proto::link::{BridgeToHost, HostToBridge};
use wartui_proto::mac;

/// How long the bridge has to announce itself, and to answer the status request.
const REPLY_TIMEOUT: Duration = super::CONNECT_NOTICE_AFTER;

/// How long a new duty runs before its speed is read: two tach windows, so one whole window ran
/// at it.
const SETTLE: Duration = Duration::from_millis(2 * TACH_WINDOW_MS as u64);

/// What a bench bridge answers.
const NO_FAN: &str = "this bridge has no carrier board, so it has no fan";

#[derive(ClapArgs)]
pub struct Args {
    /// The duty to set, 0 to 100 percent. The bridge keeps it across its reboots. Without it,
    /// prints the duty and speed.
    #[arg(value_name = "DUTY", value_parser = clap::value_parser!(u8).range(0..=100))]
    duty: Option<u8>,

    /// The bridge, as a device path, the board's address, or the end of it (`00:08`). Detected
    /// if omitted.
    #[arg(long, value_name = "PATH|MAC")]
    bridge: Option<String>,

    /// Read the remembered bridge from this file instead of the default `wartui.toml`. A file
    /// that does not load is no bridge remembered. Never written; see
    /// `crates/wartui/README.md` § "Config file".
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

pub async fn run(args: Args) -> Result<()> {
    let remembered = crate::config::remembered_bridge(args.config.as_deref());
    let named = args.bridge.as_deref();
    let port =
        crate::reset::board_to_reset(named, &remembered, discover_bridges, |found| async move {
            probe_all(&found).await
        })
        .await?;
    let memory = BridgeMemory::holding(remembered.mac).on_port(remembered.serial);
    let mut link = super::open(Some(&port), None, memory)?;

    let info =
        match tokio::time::timeout(REPLY_TIMEOUT, crate::status::wait_for_ready(&mut link)).await {
            Ok(ready) => ready?,
            Err(_) => bail!("{}", super::no_bridge_notice(named)),
        };
    has_fan(info.node_board).map_err(anyhow::Error::msg)?;

    let duty = args.duty.and_then(FanDuty::new);
    if let Some(duty) = duty {
        link.send_bulk(HostToBridge::SetFan { duty }).context("queueing the fan duty")?;
        println!("setting the fan to {duty} on bridge {}", mac::full(&info.mac));
        settle(&mut link, SETTLE).await?;
    }

    link.send_bulk(HostToBridge::GetStatus).context("queueing the status request")?;
    let status = tokio::time::timeout(REPLY_TIMEOUT, crate::status::wait_for_status(&mut link))
        .await
        .context("the bridge did not answer a status request")??;
    let BridgeToHost::Status { fan, .. } = status else {
        unreachable!("wait_for_status only returns Status")
    };
    println!("fan        {}", crate::status::fan_text(fan));
    if let Some(duty) = duty
        && fan.map(|fan| fan.duty) != Some(duty)
    {
        bail!("the bridge did not take the duty");
    }
    Ok(())
}

/// Whether `board` has a fan, or why not: only a carrier has the header.
fn has_fan(board: NodeBoard) -> Result<(), &'static str> {
    if board.is_carrier() { Ok(()) } else { Err(NO_FAN) }
}

/// Wait `wait`, printing each fault the bridge reports meanwhile, such as a duty not saved.
async fn settle(link: &mut LinkHandle, wait: Duration) -> Result<()> {
    let until = tokio::time::Instant::now() + wait;
    loop {
        match tokio::time::timeout_at(until, link.recv()).await {
            Err(_) => return Ok(()),
            Ok(Some(LinkEvent::Message(BridgeToHost::Error { message }))) => {
                println!("fault      {message}");
            }
            Ok(Some(LinkEvent::Disconnected { reason })) => bail!("{reason}"),
            Ok(Some(_)) => {}
            Ok(None) => bail!("the link closed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use wartui_proto::bus::NodeBoard;

    use super::{NO_FAN, has_fan};

    #[test]
    fn fan_cmd_takes_bridge_when_board_is_a_carrier() {
        for board in [NodeBoard::Carrier6, NodeBoard::Carrier10, NodeBoard::Carrier14] {
            assert_eq!(has_fan(board), Ok(()), "{board}");
        }
    }

    #[test]
    fn fan_cmd_refuses_before_sending_when_bridge_is_on_the_bench() {
        assert_eq!(has_fan(NodeBoard::Bench), Err(NO_FAN));
    }
}
