//! `wartui power-cycle` — switch one node off and on through the carrier board.
//!
//! The carrier's I/O expander switches each node's power, so a hung node comes back without
//! anyone reaching for it. The bridge is resolved to one board as `wartui reset` resolves it
//! (`reset::board_to_reset`), since a sweep asks whichever bridge answers first. Its `Ready`
//! names the board, which decides what `--slot` takes: the silk names, `N1` to `Nn`. A bench
//! bridge, which switches no power, and a name the board does not take are refused before
//! anything is sent.
//!
//! A node with its own USB plugged in stays powered through a cycle, since USB feeds it past
//! the carrier's switch.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use wartui_bridge::remember::BridgeMemory;
use wartui_bridge::serial::{discover_bridges, probe_all};
use wartui_bridge::{LinkEvent, LinkHandle};
use wartui_core::engine::power_cycled_text;
use wartui_proto::bus::{NodeBoard, Slot};
use wartui_proto::link::{BridgeToHost, HostToBridge, PowerOutcome};
use wartui_proto::mac;

/// How long the bridge has to announce itself, as `status` waits.
const READY_TIMEOUT: Duration = super::CONNECT_NOTICE_AFTER;

/// How long the bridge has to answer: the node's second off, and a cycle already running ahead
/// of it.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(ClapArgs)]
pub struct Args {
    /// The node, by the name the fleet table shows: `N3` on a carrier.
    #[arg(long, value_name = "NAME")]
    slot: String,

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
        match tokio::time::timeout(READY_TIMEOUT, crate::status::wait_for_ready(&mut link)).await {
            Ok(ready) => ready?,
            Err(_) => bail!("{}", super::no_bridge_notice(named)),
        };
    let board = info.node_board;
    let slot = slot_for(board, &args.slot).map_err(anyhow::Error::msg)?;

    link.send_bulk(HostToBridge::PowerCycle { slot }).context("queueing the power-cycle")?;
    println!("power-cycling {} on bridge {}", board.label(slot), mac::full(&info.mac));
    let outcome = tokio::time::timeout(ANSWER_TIMEOUT, wait_for_answer(&mut link, slot))
        .await
        .with_context(|| {
        format!("the bridge did not answer within {}s", ANSWER_TIMEOUT.as_secs())
    })??;
    let text = power_cycled_text(board, slot, outcome);
    if outcome != PowerOutcome::Done {
        bail!("{text}");
    }
    println!("{text}");
    Ok(())
}

/// The slot `name` means on `board`, or why it is refused: a bench bridge switches no power, and
/// a carrier takes only the names it drives.
fn slot_for(board: NodeBoard, name: &str) -> Result<Slot, String> {
    if !board.is_carrier() {
        return Err("this bridge has no carrier board, so it cannot switch node power".to_owned());
    }
    board.parse_label(name).ok_or_else(|| {
        format!(
            "{name:?} is not a slot on this {board}: name one of {}",
            crate::text::slot_range(board)
        )
    })
}

/// Wait for the answer about `slot`, printing each fault the bridge reports meanwhile.
async fn wait_for_answer(link: &mut LinkHandle, slot: Slot) -> Result<PowerOutcome> {
    loop {
        match link.recv().await {
            Some(LinkEvent::Message(BridgeToHost::PowerCycled { slot: answered, outcome }))
                if answered == slot =>
            {
                return Ok(outcome);
            }
            Some(LinkEvent::Message(BridgeToHost::Error { message })) => {
                println!("fault      {message}");
            }
            Some(LinkEvent::Disconnected { reason }) => bail!("{reason}"),
            Some(_) => {}
            None => bail!("the link closed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use wartui_proto::bus::{NodeBoard, Slot};

    use super::slot_for;

    #[test]
    fn power_cycle_cmd_takes_silk_name_when_board_drives_it() {
        assert_eq!(slot_for(NodeBoard::Carrier10, "N6"), Ok(Slot::new(1, 0).unwrap()));
        assert_eq!(slot_for(NodeBoard::Carrier14, "n14"), Ok(Slot::new(1, 6).unwrap()));
    }

    #[test]
    fn power_cycle_cmd_refuses_name_with_valid_range_when_board_does_not_take_it() {
        for name in ["N11", "N0", "B1", "A1", "3"] {
            let refused = slot_for(NodeBoard::Carrier10, name).expect_err(name);
            assert!(refused.contains("N1 to N10"), "{name}: {refused}");
            assert!(refused.contains("carrier-10"), "{name}: {refused}");
        }
    }

    #[test]
    fn power_cycle_cmd_refuses_before_sending_when_bridge_is_on_the_bench() {
        let refused = slot_for(NodeBoard::Bench, "A1").expect_err("no carrier");
        assert!(refused.contains("no carrier board"), "{refused}");
    }
}
