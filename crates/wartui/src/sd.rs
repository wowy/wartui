//! `wartui sd` — show the bridge's microSD card, format it, or set whether the bridge logs to it.
//!
//! `format` erases the card and writes one FAT32 partition labeled `WARTUI`
//! (`wartui_proto::sd::format`). It asks no confirmation, as `power-cycle` and `reset` do not:
//! naming the action is the confirmation. The bridge answers once the format ends, a few seconds
//! for a 64 GB card, so the command waits up to [`FORMAT_TIMEOUT`]; the transport keeps the link
//! alive meanwhile. An answer other than done is the command's error. A done format is followed
//! by a fresh `Status`, so the card line shows the new volume.
//!
//! `log` reads or sets the setting alone: whether the bridge logs to the card. The bridge keeps
//! it on its own flash, as it keeps `wardrive esp`, and this sets it as `wartui bluetooth
//! wardrive-esp` sets that: send, listen for a fault, then read the setting back from a fresh
//! `Status`.
//!
//! The bridge is resolved to one board as `wartui scan` resolves it. A `wartui run` holds the
//! port, so this runs only while none is attached; in `run`, settings' `format` row does the same.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args as ClapArgs, Subcommand};
use wartui_bridge::{LinkEvent, LinkHandle};
use wartui_core::engine::sd_formatted_text;
use wartui_proto::link::{BridgeToHost, HostToBridge, SdFormatOutcome, SdStatus};
use wartui_proto::mac;

use crate::OnOff;
use crate::text::{SdForm, sd_log_text, sd_text};

/// How long the bridge has to answer the status request.
const REPLY_TIMEOUT: Duration = super::CONNECT_NOTICE_AFTER;

/// How long the bridge has to finish a format: room past the few seconds a large card takes.
const FORMAT_TIMEOUT: Duration = Duration::from_secs(120);

/// How long `log` listens for a fault once the setting is sent, as `wartui pool` does.
const SETTING_SETTLE: Duration = Duration::from_millis(500);

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
    /// Format the card as FAT32, erasing everything on it. Asks no confirmation.
    Format {
        #[command(flatten)]
        target: Target,
    },
    /// Read or set the SD log setting: whether the bridge logs to its card. The bridge keeps it
    /// across its reboots. This is the setting; it reads no log.
    Log {
        /// `on` logs, `off` does not. Without it, prints the setting.
        #[arg(value_name = "on|off")]
        state: Option<OnOff>,

        #[command(flatten)]
        target: Target,
    },
}

/// Which bridge, shared by the bare command and its action.
#[derive(ClapArgs)]
struct Target {
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

    /// Ask the simulator instead of hardware. Its card holds no FAT32 volume until formatted.
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

/// What to ask the bridge for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wanted {
    /// Nothing: show the card.
    Show,
    /// Format it.
    Format,
}

pub async fn run(args: Args) -> Result<()> {
    let (wanted, target) = match args.action {
        None => (Wanted::Show, args.target),
        Some(Action::Format { target }) => (Wanted::Format, target),
        Some(Action::Log { state, target }) => return log(state.map(OnOff::is_on), &target).await,
    };
    let (mut link, info) = connect(&target).await?;
    if wanted == Wanted::Format {
        println!("formatting the SD card on bridge {}", mac::full(&info.mac));
    }
    for line in act(&mut link, wanted).await? {
        println!("{line}");
    }
    Ok(())
}

/// `wartui sd log`: set the log to `wanted` when given, then print the setting in force.
async fn log(wanted: Option<bool>, target: &Target) -> Result<()> {
    let (mut link, info) = connect(target).await?;
    if let Some(on) = wanted {
        let state = if on { "on" } else { "off" };
        println!("turning sd log {state} on bridge {}", mac::full(&info.mac));
    }
    let (line, taken) = set_log(&mut link, wanted).await?;
    println!("{line}");
    if !taken {
        bail!("the bridge did not take the setting");
    }
    Ok(())
}

/// Send the log setting `wanted` when given, then return the `sd log` line from a fresh status
/// and whether the bridge holds what was wanted.
async fn set_log(link: &mut LinkHandle, wanted: Option<bool>) -> Result<(String, bool)> {
    if let Some(on) = wanted {
        link.send_bulk(HostToBridge::SetSdLog { on }).context("queueing the setting")?;
        crate::settle(link, SETTING_SETTLE).await?;
    }
    let sd = sd_status(link).await?;
    let taken = wanted.is_none_or(|on| sd.map(|sd| sd.log.on) == Some(on));
    Ok((format!("sd log     {}", sd_log_text(sd)), taken))
}

/// The bridge `target` names, connected and announced.
async fn connect(target: &Target) -> Result<(LinkHandle, wartui_bridge::BridgeInfo)> {
    if target.sim.is_none() {
        return super::connect_one(
            target.bridge.as_deref(),
            target.link_flag.link,
            target.config.as_deref(),
        )
        .await;
    }
    let mut link = super::open(None, target.sim, crate::config::Link::Usb, super::memory(None))?;
    let info = crate::status::wait_for_ready(&mut link, crate::config::Link::Usb, REPLY_TIMEOUT)
        .await?
        .context("the simulator did not announce itself")?;
    Ok((link, info))
}

/// Carry out `wanted` on the bridge behind `link`, and return the lines that report it. Fails
/// with the bridge's words when a format does not end done.
async fn act(link: &mut LinkHandle, wanted: Wanted) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    if wanted == Wanted::Format {
        link.send_bulk(HostToBridge::FormatSd).context("queueing the format")?;
        let outcome = tokio::time::timeout(FORMAT_TIMEOUT, wait_for_answer(link))
            .await
            .with_context(|| {
                format!("the bridge did not finish the format within {}s", FORMAT_TIMEOUT.as_secs())
            })??;
        let text = sd_formatted_text(outcome);
        if outcome != SdFormatOutcome::Done {
            bail!("{text}");
        }
        lines.push(text);
    }
    lines.push(card_line(link).await?);
    Ok(lines)
}

/// The card, from a fresh status request.
async fn card_line(link: &mut LinkHandle) -> Result<String> {
    Ok(format!("sd card    {}", sd_text(sd_status(link).await?, SdForm::Line)))
}

/// The card as a fresh status request reports it.
async fn sd_status(link: &mut LinkHandle) -> Result<Option<SdStatus>> {
    link.send_bulk(HostToBridge::GetStatus).context("queueing the status request")?;
    let status = tokio::time::timeout(REPLY_TIMEOUT, crate::status::wait_for_status(link))
        .await
        .context("the bridge did not answer a status request")??;
    let BridgeToHost::Status { sd, .. } = status else {
        unreachable!("wait_for_status only returns Status")
    };
    Ok(sd)
}

/// Wait for the format's answer, printing each fault the bridge reports meanwhile.
async fn wait_for_answer(link: &mut LinkHandle) -> Result<SdFormatOutcome> {
    loop {
        match link.recv().await {
            Some(LinkEvent::Message(BridgeToHost::SdFormatted { outcome })) => return Ok(outcome),
            Some(LinkEvent::Message(BridgeToHost::Error { message })) => {
                println!("fault      {message}");
            }
            Some(LinkEvent::Disconnected { reason, .. }) => bail!("{reason}"),
            Some(_) => {}
            None => bail!("the link closed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use wartui_bridge::sim::{SimConfig, SimTransport};
    use wartui_proto::link::HostToBridge;

    use super::{LinkHandle, Wanted, act, set_log};

    /// A simulated bridge, connected.
    async fn sim() -> LinkHandle {
        let mut link = SimTransport::new(SimConfig::default()).start().expect("the simulator");
        let ready = crate::status::wait_for_ready(
            &mut link,
            crate::config::Link::Usb,
            super::REPLY_TIMEOUT,
        );
        ready.await.expect("the simulator connects").expect("in time");
        link
    }

    #[tokio::test(start_paused = true)]
    async fn sd_cmd_shows_card_without_formatting_when_given_no_action() {
        let mut link = sim().await;
        let lines = act(&mut link, Wanted::Show).await.expect("the status");
        assert_eq!(lines, ["sd card    29.7 GB, not FAT32"]);
    }

    #[tokio::test(start_paused = true)]
    async fn sd_cmd_reports_free_space_when_format_is_done() {
        let mut link = sim().await;
        let lines = act(&mut link, Wanted::Format).await.expect("the format");
        assert_eq!(lines[0], "SD card formatted", "{lines:?}");
        assert!(lines[1].starts_with("sd card    "), "{lines:?}");
        assert!(lines[1].ends_with(" of 29.7 GB free"), "{lines:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn sd_cmd_shows_log_setting_when_given_no_state() {
        let mut link = sim().await;
        let shown = set_log(&mut link, None).await.expect("the status");
        assert_eq!(shown, ("sd log     on · waiting for a card".to_owned(), true));
    }

    #[tokio::test(start_paused = true)]
    async fn sd_cmd_sets_log_and_reads_it_back_when_given_a_state() {
        let mut link = sim().await;
        let off = set_log(&mut link, Some(false)).await.expect("the setting");
        assert_eq!(off, ("sd log     off".to_owned(), true));
        let on = set_log(&mut link, Some(true)).await.expect("the setting");
        assert_eq!(on, ("sd log     on · waiting for a card".to_owned(), true));
    }

    #[tokio::test(start_paused = true)]
    async fn sd_cmd_shows_log_writing_file_when_card_is_formatted() {
        let mut link = sim().await;
        act(&mut link, Wanted::Format).await.expect("the format");
        let (line, _) = set_log(&mut link, None).await.expect("the status");
        assert!(line.starts_with("sd log     on · writing LOG/00000001.BIN · "), "{line}");
    }

    #[tokio::test(start_paused = true)]
    async fn sd_cmd_fails_with_bridges_words_when_a_format_is_already_running() {
        let mut link = sim().await;
        link.send_bulk(HostToBridge::FormatSd).expect("queued");
        let refused = act(&mut link, Wanted::Format).await.expect_err("busy");
        assert_eq!(refused.to_string(), "the SD card is already being formatted");
    }
}
