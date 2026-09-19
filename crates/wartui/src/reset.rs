//! `wartui reset` — reboot the bridge without reaching for `espflash`.
//!
//! The failure this answers is transmit-only: the USB Serial/JTAG transmit endpoint
//! can stop draining while the receive endpoint carries on, so the bridge goes on
//! reading commands it has no way to answer. From the host that is
//! indistinguishable from a dead board, but the receive path is never the problem —
//! a single `Reset` frame down the same wire reboots a wedged board
//! (`docs/phase-3-findings.md`). So this is the first thing to reach for, and
//! `espflash`, which drives DTR/RTS and does not care whether the firmware is
//! alive, is what is left when even this does not answer.
//!
//! Rebooting is confirmed by uptime and by uptime alone; [`rebooted`] is the rule.
//! [`FRESH_UPTIME_MS`] covers the one case with no earlier figure to compare
//! against: a bridge that was wedged and never spoke at all.
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use wartui_bridge::{BridgeInfo, LinkEvent};
use wartui_proto::link::{BridgeToHost, HostToBridge};

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
    /// The bridge, as a device path or as the board's address. Detected if omitted.
    #[arg(long, value_name = "PATH|MAC")]
    bridge: Option<String>,
}

pub async fn run(args: Args) -> Result<()> {
    let mut link = super::open(args.bridge.as_deref(), None, 0)?;

    // Sent immediately, without waiting to be told the bridge is there: the case
    // this command is for is the one where nothing ever announces itself, and the
    // writer thread is ready as soon as the port is open.
    link.send_urgent(HostToBridge::Reset).context("queueing the reset")?;
    println!("reset sent to {}", args.bridge.as_deref().unwrap_or("the board that was detected"));

    match tokio::time::timeout(RECOVERY_TIMEOUT, wait_for_reboot(&mut link)).await {
        Ok(Ok((info, uptime_ms))) => {
            report(info.as_ref(), uptime_ms);
            Ok(())
        }
        Ok(Err(err)) => Err(err),
        Err(_) => bail!("{}", unreachable_notice(args.bridge.as_deref())),
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
        Some(info) => println!(
            "back up: {} on {:?}, firmware {}",
            super::mac(&info.mac),
            info.chip,
            info.fw_version
        ),
        // Reachable when the bridge was announced on a connection this command did
        // not open: the transport reports one `Connected` per connection.
        None => println!("back up"),
    }
    println!("uptime     {uptime_ms}ms, so it did reboot");
    if let Some(info) = info {
        println!("{}", super::last_reset_line(info));
    }
}

/// What to say when even a reset frame goes unanswered.
///
/// `espflash` takes a path and nothing else, so a board named by its address is
/// resolved to one here rather than quoted into a line that will not run.
fn unreachable_notice(bridge: Option<&str>) -> String {
    let seconds = RECOVERY_TIMEOUT.as_secs();
    let device = bridge.and_then(super::named_device);
    let (where_, espflash) = match (bridge, device.as_deref()) {
        (Some(_), Some(path)) => (format!("on {path}"), format!("espflash reset --port {path}")),
        (Some(name), None) => (
            format!("named {name}, which no attached board answers to"),
            "espflash reset".to_owned(),
        ),
        (None, _) => ("on the board that was detected".to_owned(), "espflash reset".to_owned()),
    };
    [
        format!("the bridge {where_} did not come back within {seconds}s."),
        "A reset frame reaches the bridge through its receive path, so this means that".to_owned(),
        "path is not running either — the firmware is hung, or this is not a bridge.".to_owned(),
        format!("Reset it over USB instead, which needs no firmware at all: {espflash}"),
        "`wartui ports` lists what is attached; `--log-file` records what the transport tried."
            .to_owned(),
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::{FRESH_UPTIME_MS, rebooted, unreachable_notice};

    #[test]
    fn a_clock_that_went_backwards_is_the_reboot() {
        // The ordinary case: it answered before the reset, so no threshold.
        assert!(rebooted(200, Some(10_800_000)), "hours of uptime replaced by a fresh life");
        assert!(!rebooted(10_800_050, Some(10_800_000)), "the same life, fifty ms later");
        // The case a bare freshness test gets wrong: it answered, so it is compared.
        assert!(!rebooted(2_100, Some(2_000)), "young, but still the same life");
        assert!(rebooted(150, Some(2_000)), "young, and then younger still");
    }

    #[test]
    fn a_bridge_that_never_spoke_falls_back_to_freshness() {
        // The wedged case: nothing was heard from the old life to compare against.
        assert!(rebooted(0, None));
        assert!(rebooted(FRESH_UPTIME_MS, None));
        assert!(!rebooted(FRESH_UPTIME_MS + 1, None));
    }

    #[test]
    fn the_notice_names_the_board_and_a_remedy_that_needs_no_firmware() {
        let notice = unreachable_notice(Some("/dev/ttyACM0"));
        assert!(notice.contains("on /dev/ttyACM0 did not come back within 10s"), "{notice}");
        assert!(notice.contains("espflash reset --port /dev/ttyACM0"), "{notice}");
    }

    #[test]
    fn without_a_named_board_it_still_reads_as_a_sentence() {
        let notice = unreachable_notice(None);
        assert!(notice.contains("on the board that was detected"), "{notice}");
        // No dangling `--port` with nothing after it.
        assert!(!notice.contains("--port"), "{notice}");
    }
}
