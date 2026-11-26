//! `wartui fan` — read or set the carrier's lid fan duty.
//!
//! The bridge keeps the duty on its own flash and boots into it, so it holds across the bridge's
//! reboots and with no host attached. `wartui.toml` holds no copy. The bridge is resolved to one
//! board as `wartui power-cycle` resolves it. A bridge with no carrier has no fan, and is refused
//! before anything is sent.
//!
//! With a duty, the bridge applies it at once and the command waits two of its tach windows
//! (`wartui_app::ops::FAN_SETTLE`), so the speed it prints is read at the new duty.

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Args as ClapArgs;
use wartui_app::ops;
use wartui_proto::bus::NodeBoard;
use wartui_proto::fan::FanDuty;
use wartui_proto::mac;

use crate::status::{NO_STATUS, link_error, print_fault};

/// What a bridge with no carrier answers.
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

    #[command(flatten)]
    link_flag: crate::config::LinkFlag,

    /// Read the remembered bridge from this file instead of the default `wartui.toml`. A file
    /// that does not load is no bridge remembered. Never written; see
    /// `crates/wartui/README.md` § "Config file".
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

pub async fn run(args: Args) -> Result<()> {
    let (mut link, info) =
        super::connect_one(args.bridge.as_deref(), args.link_flag.link, args.config.as_deref())
            .await?;
    has_fan(info.node_board).map_err(anyhow::Error::msg)?;

    let duty = args.duty.and_then(FanDuty::new);
    let (status, taken) = match duty {
        Some(duty) => {
            println!("setting the fan to {duty} on bridge {}", mac::full(&info.mac));
            let set = ops::set_fan(&mut link, duty, &mut print_fault).await;
            let set = set.map_err(|error| link_error(error, NO_STATUS))?;
            (set.status, set.taken)
        }
        None => {
            let status = ops::status(&mut link, ops::REPLY_TIMEOUT, &mut print_fault).await;
            (status.map_err(|error| link_error(error, NO_STATUS))?, true)
        }
    };
    println!("fan        {}", crate::status::fan_text(status.fan));
    if !taken {
        bail!("the bridge did not take the duty");
    }
    Ok(())
}

/// Whether `board` has a fan, or why not: only a carrier has the header.
fn has_fan(board: NodeBoard) -> Result<(), &'static str> {
    if board.is_carrier() { Ok(()) } else { Err(NO_FAN) }
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
    fn fan_cmd_refuses_before_sending_when_bridge_has_no_carrier() {
        assert_eq!(has_fan(NodeBoard::NoCarrier), Err(NO_FAN));
    }
}
