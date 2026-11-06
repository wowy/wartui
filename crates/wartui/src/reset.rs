//! `wartui reset` — reboot the bridge without reaching for `espflash`.
//!
//! A bridge that has stopped answering may still be reading: from the host, one with
//! no way to answer is indistinguishable from a dead board, and a single `Reset` frame
//! down the same wire reboots it. So this is the first thing to reach for, and a power
//! cycle is what is left when even this does not answer.
//!
//! Transmitting before anything has identified itself is what makes this command
//! work and what makes it the one command that must never be pointed at a guess.
//! `Reset` and the status polls behind it are several frames. A CH343 carries every one
//! to whatever is behind it, but a path can name a node, whose USB Serial/JTAG absorbs
//! one: the rest sit in the tty's output queue until `close` gives up on them thirty
//! seconds later, so `wartui reset` aimed at a node takes about that long to say it
//! failed. That is the cost of asking a board
//! that answers nothing, and the reason `run`'s connection, which asks a port that is not a
//! CH343 with one frame, does not share this command's license.
//!
//! **The P4 comes back on the same port, deaf until asked.** Its CH343 stays on the bus,
//! so the connection holds through the reboot and reads the bootloader's log as garbage.
//! The rebooted P4 writes nothing until a host frame arrives, which the status polls are.
//!
//! Rebooting is confirmed by uptime and by uptime alone; [`rebooted`] is the rule.
//! [`FRESH_UPTIME_MS`] covers the one case with no earlier figure to compare
//! against: a bridge that was wedged and never spoke at all.
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use wartui_bridge::ports::PortCandidate;
use wartui_bridge::remember::BridgeMemory;
use wartui_bridge::serial::{self, BridgeSpec, Probed, discover_bridges, probe_all};
use wartui_bridge::{BridgeInfo, LinkEvent, TransportError};
use wartui_proto::link::{BridgeToHost, HostToBridge};
use wartui_proto::mac;

/// How long to keep asking before giving up on the board entirely.
///
/// Longer than [`super::CONNECT_NOTICE_AFTER`] on purpose: a few seconds of silence
/// after asking for a reboot is expected, and the transport's own six-second give-up
/// has to run at least once underneath it.
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// An uptime at or below this is a bridge that has just restarted.
///
/// Slack for the reboot, the radio coming up and the round trip. Only consulted when
/// nothing was heard before the reset; otherwise [`rebooted`] compares the two
/// uptimes and needs no threshold.
const FRESH_UPTIME_MS: u32 = 10_000;

/// How often to re-ask for a status while waiting for it to come back.
const POLL: Duration = Duration::from_millis(500);

/// How long to keep listening for the announcement after the proof arrives.
///
/// The uptime proves the reboot and can win the race against the `Ready` carrying
/// the reset cause — measured at 0 ms uptime on a C6. Waiting a moment for the more
/// interesting of the two costs nothing when it has already arrived.
const ANNOUNCE_GRACE: Duration = Duration::from_millis(750);

#[derive(ClapArgs)]
pub struct Args {
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
    // Resolved to one board here rather than left to the transport, which sweeps.
    // A sweep sends this command's `Reset` into every CH343 attached until one
    // answers, and with two attached that reboots another fleet's bridge. So this
    // command gets an unambiguous board or none, and says which it wanted. It opens that
    // board by path: the reboot leaves its CH343 where it was.
    let remembered = crate::config::remembered_bridge(args.config.as_deref());
    let named = args.bridge.as_deref();
    let board = board_to_reset(named, &remembered, discover_bridges, |found| async move {
        probe_all(&found).await
    })
    .await?;
    let memory = BridgeMemory::holding(remembered.mac).on_port(remembered.serial);
    let mut link = super::open(Some(&board), None, memory)?;

    // Sent immediately, without waiting to be told the bridge is there: the case
    // this command is for is the one where nothing ever announces itself, and the
    // writer thread is ready as soon as the port is open.
    link.send_urgent(HostToBridge::Reset).context("queueing the reset")?;
    println!("reset sent to {board}");

    match tokio::time::timeout(RECOVERY_TIMEOUT, wait_for_reboot(&mut link)).await {
        Ok(Ok((info, uptime_ms))) => {
            report(info.as_ref(), uptime_ms);
            Ok(())
        }
        Ok(Err(err)) => Err(err),
        Err(_) => bail!("{}", unreachable_notice(&board)),
    }
}

/// The one board to send `Reset` to, as the path to open it by. `power-cycle` resolves its bridge
/// the same way, so its `PowerCycle` reaches no other bridge.
///
/// Unnamed, it is the CH343 the bridge was remembered on, or the only one attached. An
/// address, whole or its end, is matched by asking each CH343 `list` reads (`probe`). One no
/// attached bridge answers with, a node's included, or a tail several share is refused before
/// anything is opened, rather than left to a transport that would wait out the whole timeout
/// saying nothing. A wedged bridge answers no probe, so an address or tail that names the
/// remembered bridge goes to its remembered CH343 without one. A path is opened as given, with
/// nothing listed: whether it exists is the question the open asks.
pub(crate) async fn board_to_reset<Probe, Asked>(
    named: Option<&str>,
    remembered: &crate::config::Bridge,
    list: impl FnOnce() -> Result<Vec<PortCandidate>, TransportError>,
    probe: Probe,
) -> Result<String, TransportError>
where
    Probe: FnOnce(Vec<PortCandidate>) -> Asked,
    Asked: Future<Output = Vec<Probed>>,
{
    match named.map(super::spec) {
        Some(BridgeSpec::Path(path)) => Ok(path),
        Some(spec) => {
            let attached = list()?;
            if remembered.mac.is_some_and(|mac| spec.matches_address(&mac))
                && let Some(port) = attached
                    .iter()
                    .find(|c| serial::is_remembered_port(c, remembered.serial.as_deref()))
            {
                return Ok(port.path.clone());
            }
            let probed = probe(attached).await;
            serial::named_board(&probed, &spec).map(|board| board.candidate.path.clone())
        }
        None => {
            let attached = list()?;
            serial::unambiguous_bridge(&attached, remembered.serial.as_deref())
                .map(|port| port.path.clone())
        }
    }
}

/// Poll for a status until one comes back reporting a bridge that restarted.
///
/// The `Connected` from the life being *replaced* is not what is wanted and is
/// not kept: it carries the old life's reset cause, and printing that under
/// "back up" would answer the question this command exists to ask with the
/// answer to the previous one.
async fn wait_for_reboot(
    link: &mut wartui_bridge::LinkHandle,
) -> Result<(Option<BridgeInfo>, u32)> {
    let mut seen: Option<BridgeInfo> = None;
    let mut proof: Option<u32> = None;
    // The highest uptime seen from the life that is being replaced.
    let mut before: Option<u32> = None;
    let mut poll = tokio::time::interval(POLL);
    let grace = tokio::time::sleep(Duration::MAX);
    tokio::pin!(grace);
    loop {
        tokio::select! {
            _ = poll.tick() => {
                // Ignored rather than propagated: until the bridge is back the
                // link may be mid-reconnect, and a queue that is not there yet
                // is what this loop is waiting out.
                let _ = link.send_bulk(HostToBridge::GetStatus);
            }
            // Armed once, when the reboot is proven, and never re-armed; until
            // then it is a sleep that never finishes. Re-arming it on each
            // status would make it unreachable, because the poll interval is
            // shorter than the grace and a bridge answers in milliseconds.
            () = &mut grace => return Ok((seen, proof.unwrap_or_default())),
            event = link.recv() => match event {
                Some(LinkEvent::Connected(info)) => {
                    if !rebooted(info.uptime_ms, before) {
                        before = before.max(Some(info.uptime_ms));
                        continue;
                    }
                    seen = Some(info);
                    if proof.is_some() {
                        return Ok((seen, proof.unwrap_or_default()));
                    }
                }
                Some(LinkEvent::Message(BridgeToHost::Status { uptime_ms, .. })) => {
                    if !rebooted(uptime_ms, before) {
                        before = before.max(Some(uptime_ms));
                        continue;
                    }
                    if seen.is_some() {
                        return Ok((seen, uptime_ms));
                    }
                    if proof.is_none() {
                        grace.as_mut().reset(tokio::time::Instant::now() + ANNOUNCE_GRACE);
                    }
                    proof = Some(uptime_ms);
                }
                // A disconnect is not a failure here. The reset may have taken
                // the port with it, and the transport reopens on its own.
                Some(_) => {}
                None => bail!("the link closed"),
            },
        }
    }
}

/// Whether an uptime belongs to a life that started after the one before it.
///
/// A clock that went backwards is a reboot and cannot be anything else, which
/// is the whole test whenever the previous life was heard from. When it was
/// not — the wedged bridge that answered nothing until the reset landed — there
/// is nothing to compare against and freshness is the only evidence available.
fn rebooted(uptime_ms: u32, before: Option<u32>) -> bool {
    match before {
        Some(previous) => uptime_ms < previous,
        None => uptime_ms <= FRESH_UPTIME_MS,
    }
}

fn report(info: Option<&BridgeInfo>, uptime_ms: u32) {
    match info {
        Some(info) => println!("back up: {}, firmware {}", mac::full(&info.mac), info.fw_version),
        // Reachable when the bridge was announced on a connection this command did
        // not open: the transport reports one `Connected` per connection.
        None => println!("back up"),
    }
    println!("uptime     {uptime_ms}ms, so it did reboot");
    if let Some(info) = info {
        println!("{}", super::last_reset_line(info));
    }
}

/// What to say when even a reset frame goes unanswered, given the address or path it went to.
fn unreachable_notice(board: &str) -> String {
    let seconds = RECOVERY_TIMEOUT.as_secs();
    [
        format!("the bridge {board} did not come back within {seconds}s."),
        "A reset frame reaches the bridge through its receive path, so this means that".to_owned(),
        "path is not running either — the firmware is hung, or this is not a bridge.".to_owned(),
        "Power-cycle it, which needs no firmware at all.".to_owned(),
        "`wartui ports` lists what is attached; `--log-file` records what the transport tried."
            .to_owned(),
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use wartui_bridge::TransportError;
    use wartui_bridge::ports::{CH343_PID, CH343_VID, PortCandidate, candidate};
    use wartui_bridge::serial::Probed;
    use wartui_proto::mac;

    use super::{FRESH_UPTIME_MS, board_to_reset, rebooted, unreachable_notice};
    use crate::config::Bridge;

    const BRIDGE_MAC: &str = "10:BD:A3:EC:44:C0";
    const TWIN_MAC: &str = "38:44:BE:12:44:C0";
    const NODE_MAC: &str = "02:00:5E:10:9D:24";

    fn ch343(path: &str, serial: &str) -> PortCandidate {
        candidate(path, Some(CH343_VID), Some(CH343_PID), Some(serial))
    }

    /// What `discover_bridges` reads with one CH343 attached.
    fn attached() -> Result<Vec<PortCandidate>, TransportError> {
        Ok(vec![ch343("/dev/ttyACM0", "5CF7153520")])
    }

    /// What `discover_bridges` reads with two CH343s attached.
    fn twins() -> Result<Vec<PortCandidate>, TransportError> {
        Ok(vec![ch343("/dev/ttyACM0", "5CF7153520"), ch343("/dev/ttyACM1", "5CF7153999")])
    }

    /// A probe that hears `macs` from the CH343s, in order: `None` is one that stays silent.
    fn answers(
        macs: &'static [Option<&'static str>],
    ) -> impl FnOnce(Vec<PortCandidate>) -> std::future::Ready<Vec<Probed>> {
        move |found| {
            let probed = found
                .into_iter()
                .zip(macs)
                .map(|(candidate, mac)| Probed {
                    candidate,
                    opened: true,
                    mac: mac.and_then(mac::parse),
                })
                .collect();
            std::future::ready(probed)
        }
    }

    /// A probe that must not be needed.
    fn unasked(_: Vec<PortCandidate>) -> std::future::Ready<Vec<Probed>> {
        panic!("nothing to probe")
    }

    fn remembered(mac: &str, serial: &str) -> Bridge {
        Bridge { remember: None, mac: mac::parse(mac), serial: Some(serial.to_owned()) }
    }

    #[tokio::test]
    async fn reset_cmd_opens_answering_port_when_bridge_is_named_in_full() {
        let probe = answers(&[Some(BRIDGE_MAC)]);
        let board = board_to_reset(Some(BRIDGE_MAC), &Bridge::default(), attached, probe).await;
        assert_eq!(board.expect("that bridge"), "/dev/ttyACM0");
    }

    #[tokio::test]
    async fn reset_cmd_opens_answering_port_when_one_bridge_ends_with_tail() {
        let probe = answers(&[None, Some(BRIDGE_MAC)]);
        let board = board_to_reset(Some("44:c0"), &Bridge::default(), twins, probe).await;
        assert_eq!(board.expect("that bridge"), "/dev/ttyACM1");
    }

    #[tokio::test]
    async fn reset_cmd_refuses_tail_when_several_bridges_end_with_it() {
        let probe = answers(&[Some(BRIDGE_MAC), Some(TWIN_MAC)]);
        let refused = board_to_reset(Some("44:C0"), &Bridge::default(), twins, probe).await;
        let refused = refused.expect_err("ambiguous");
        assert!(matches!(refused, TransportError::AmbiguousName { .. }), "{refused}");
    }

    #[tokio::test]
    async fn reset_cmd_opens_remembered_port_without_probe_when_bridge_is_named_in_full() {
        // The board this command is for answers nothing, so its own address cannot be asked.
        let known = remembered(BRIDGE_MAC, "5CF7153999");
        let board = board_to_reset(Some(BRIDGE_MAC), &known, twins, unasked).await;
        assert_eq!(board.expect("the remembered port"), "/dev/ttyACM1");
    }

    #[tokio::test]
    async fn reset_cmd_opens_remembered_port_without_probe_when_bridge_is_named_by_tail() {
        // How an operator names the bridge, and a wedged one answers no probe either way.
        let known = remembered(BRIDGE_MAC, "5CF7153999");
        let board = board_to_reset(Some("44:C0"), &known, twins, unasked).await;
        assert_eq!(board.expect("the remembered port"), "/dev/ttyACM1");
    }

    #[tokio::test]
    async fn reset_cmd_probes_when_tail_does_not_end_remembered_address() {
        let known = remembered(BRIDGE_MAC, "5CF7153999");
        let probe = answers(&[Some(TWIN_MAC), None]);
        let board = board_to_reset(Some("12:44:C0"), &known, twins, probe).await;
        assert_eq!(board.expect("the twin"), "/dev/ttyACM0");
    }

    #[tokio::test]
    async fn reset_cmd_picks_sole_ch343_when_bridge_is_not_named() {
        let board = board_to_reset(None, &Bridge::default(), attached, unasked).await;
        assert_eq!(board.expect("the only bridge"), "/dev/ttyACM0");
    }

    #[tokio::test]
    async fn reset_cmd_picks_remembered_port_when_several_ch343s_are_attached() {
        let known = remembered(TWIN_MAC, "5CF7153999");
        let board = board_to_reset(None, &known, twins, unasked).await;
        assert_eq!(board.expect("the remembered one"), "/dev/ttyACM1");
    }

    #[tokio::test]
    async fn reset_cmd_refuses_unnamed_bridge_when_several_ch343s_are_unremembered() {
        let refused = board_to_reset(None, &Bridge::default(), twins, unasked).await;
        let refused = refused.expect_err("no way to tell");
        assert!(matches!(refused, TransportError::AmbiguousBridge { .. }), "{refused}");
    }

    #[tokio::test]
    async fn reset_cmd_refuses_address_when_only_a_node_carries_it() {
        // A node is never probed, so its address names nothing here, and sending to it would
        // reboot it.
        let probe = answers(&[Some(BRIDGE_MAC)]);
        let refused = board_to_reset(Some(NODE_MAC), &Bridge::default(), attached, probe).await;
        let refused = refused.expect_err("not a bridge");
        assert!(matches!(refused, TransportError::NoSuchBridge { .. }), "{refused}");
        assert!(refused.to_string().contains(NODE_MAC), "{refused}");
    }

    #[tokio::test]
    async fn reset_cmd_refuses_address_when_nothing_attached_answers_with_it() {
        let probe = answers(&[None]);
        let named = Some("AA:BB:CC:DD:EE:FF");
        let refused = board_to_reset(named, &Bridge::default(), attached, probe).await;
        let refused = refused.expect_err("not attached");
        assert!(matches!(refused, TransportError::NoSuchBridge { .. }), "{refused}");
    }

    #[tokio::test]
    async fn reset_cmd_keeps_path_without_listing_when_named_by_path() {
        let list = || panic!("nothing to list");
        let board = board_to_reset(Some("/dev/ttyACM9"), &Bridge::default(), list, unasked).await;
        assert_eq!(board.expect("the path as given"), "/dev/ttyACM9");
    }

    #[test]
    fn reset_cmd_identifies_reboot_when_uptime_decreases_relative_to_previous_state() {
        // The ordinary case: it answered before the reset, so no threshold.
        assert!(rebooted(200, Some(10_800_000)), "hours of uptime replaced by a fresh life");
        assert!(!rebooted(10_800_050, Some(10_800_000)), "the same life, fifty ms later");
        // The case a bare freshness test gets wrong: it answered, so it is compared.
        assert!(!rebooted(2_100, Some(2_000)), "young, but still the same life");
        assert!(rebooted(150, Some(2_000)), "young, and then younger still");
    }

    #[test]
    fn reset_cmd_assumes_clean_state_when_bridge_has_no_prior_history() {
        // The wedged case: nothing was heard from the old life to compare against.
        assert!(rebooted(0, None));
        assert!(rebooted(FRESH_UPTIME_MS, None));
        assert!(!rebooted(FRESH_UPTIME_MS + 1, None));
    }

    #[test]
    fn reset_cmd_formats_unreachable_notice_with_board_when_board_goes_silent() {
        let notice = unreachable_notice("/dev/ttyACM0");
        assert!(notice.contains("/dev/ttyACM0 did not come back within 10s"), "{notice}");
        assert!(notice.contains("Power-cycle"), "{notice}");
    }
}
