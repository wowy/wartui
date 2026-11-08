//! `wartui bluetooth` — read or set the slot whose node scans Bluetooth, which the bridge holds.
//!
//! The bridge keeps the slot on its own flash beside the channel pool and boots into it, so it
//! holds across the bridge's reboots and with no host attached, and `run`'s planner gives the scan
//! to the node heartbeating from that slot. `wartui.toml` holds no copy. The bridge is resolved to
//! one board as `wartui fan` resolves it. Its `Ready` names the board, which decides the names a
//! slot takes: `N1` to `Nn` on a carrier. `none`, which takes the scan off the fleet, is taken on
//! any P4, carrier or not.
//!
//! With a slot or `none`, the command waits a moment before it reads the slot back, so a failed
//! save's fault prints first.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use wartui_proto::bus::{NodeBoard, Slot};
use wartui_proto::link::{BridgeToHost, HostToBridge};
use wartui_proto::mac;

/// How long the bridge has to answer the status request.
const REPLY_TIMEOUT: Duration = super::CONNECT_NOTICE_AFTER;

/// How long to listen for a fault after setting the slot.
const SETTLE: Duration = Duration::from_millis(500);

/// Why a slot name is refused on a bridge with no carrier.
const NO_CARRIER: &str = "this bridge has no carrier board, so it has no slot to give the \
                          bluetooth scan; `none` takes it off the fleet";

#[derive(ClapArgs)]
pub struct Args {
    /// The slot whose node scans Bluetooth, by the name the fleet table shows (`N3`), or `none`.
    /// The bridge keeps it across its reboots, and whichever node heartbeats from the slot scans.
    /// Without it, prints the slot.
    #[arg(value_name = "SLOT|none")]
    slot: Option<String>,

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
    let (mut link, info) =
        super::connect_one(args.bridge.as_deref(), args.config.as_deref()).await?;
    let board = info.node_board;

    let wanted = args
        .slot
        .as_deref()
        .map(|name| slot_for(board, name))
        .transpose()
        .map_err(anyhow::Error::msg)?;
    if let Some(slot) = wanted {
        link.send_bulk(HostToBridge::SetBle { slot }).context("queueing the bluetooth slot")?;
        let bridge = mac::full(&info.mac);
        match slot {
            Some(slot) => {
                println!("giving the bluetooth scan to {} on bridge {bridge}", board.label(slot));
            }
            None => println!("taking the bluetooth scan off the fleet on bridge {bridge}"),
        }
        super::settle(&mut link, SETTLE).await?;
    }

    link.send_bulk(HostToBridge::GetStatus).context("queueing the status request")?;
    let status = tokio::time::timeout(REPLY_TIMEOUT, crate::status::wait_for_status(&mut link))
        .await
        .context("the bridge did not answer a status request")??;
    let BridgeToHost::Status { ble, .. } = status else {
        unreachable!("wait_for_status only returns Status")
    };
    println!("bluetooth  {}", crate::status::ble_text(board, ble));
    if wanted.is_some_and(|wanted| wanted != ble) {
        bail!("the bridge did not take the bluetooth slot");
    }
    Ok(())
}

/// What `name` asks the bridge to hold on `board`: `None` for `none`, on any board, or the slot a
/// carrier takes by that name. Either case.
fn slot_for(board: NodeBoard, name: &str) -> Result<Option<Slot>, String> {
    if name.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    crate::text::carrier_slot_named(board, name, NO_CARRIER).map(Some)
}

#[cfg(test)]
mod tests {
    use wartui_proto::bus::{NodeBoard, Slot};

    use super::slot_for;

    #[test]
    fn bluetooth_cmd_takes_none_when_board_is_carrier_or_not() {
        for board in [NodeBoard::Carrier14, NodeBoard::NoCarrier] {
            assert_eq!(slot_for(board, "none"), Ok(None), "{board}");
            assert_eq!(slot_for(board, "NONE"), Ok(None), "{board}");
        }
    }

    #[test]
    fn bluetooth_cmd_takes_silk_name_when_board_drives_it() {
        assert_eq!(slot_for(NodeBoard::Carrier14, "N3"), Ok(Slot::new(0, 2)));
        assert_eq!(slot_for(NodeBoard::Carrier14, "n14"), Ok(Slot::new(1, 6)));
    }

    #[test]
    fn bluetooth_cmd_refuses_name_with_valid_range_when_board_does_not_take_it() {
        for name in ["N15", "N0", "B1", "3", "off"] {
            let refused = slot_for(NodeBoard::Carrier14, name).expect_err(name);
            assert!(refused.contains("N1 to N14"), "{name}: {refused}");
        }
    }

    #[test]
    fn bluetooth_cmd_refuses_slot_name_when_bridge_has_no_carrier() {
        let refused = slot_for(NodeBoard::NoCarrier, "N1").expect_err("no carrier");
        assert!(refused.contains("no carrier board"), "{refused}");
    }
}
