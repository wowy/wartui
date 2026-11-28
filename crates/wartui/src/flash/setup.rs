//! Choosing the blank P4 `flash-bridge` offers to set up, asking the operator, and reading the
//! loader it then runs.
//!
//! **A silent CH343 is offered only after `board-info` names an ESP32-P4.** A CH343 that does
//! not answer the bridge probe is a blank P4, or a node's adapter, or any other serial device;
//! the probe alone cannot tell them apart, and setting up the wrong one erases it. So each
//! silent one is asked `espflash board-info` first, and only one that reports `esp32p4` is a
//! candidate.
//!
//! **A remembered bridge that is silent is never offered.** It is a board known to have been
//! the bridge, and a bridge that stopped answering is broken or absent, not blank; a setup
//! would wipe its settings and Bluetooth bonds. The same holds for a bridge named by address:
//! the probe heard no address from a blank P4, so an address cannot name one. Only a
//! `--bridge` path can, and only when no other bridge is in the way.
//!
//! **The bridge is known by its C6's address afterwards.** The P4's own address is not the one
//! the running bridge reports, so the address read here names the board only until the first
//! flash completes; the `[bridge]` memory is written from the address the new firmware answers
//! with.
//!
//! **The loader's lines are read from the CH343 as they arrive.** They end in `\r\n` and sit
//! among ESP-IDF log lines that carry ANSI color, so a line counts only when it starts with
//! `wartui-loader: ` and parses; everything else is ignored.
//!
//! Some items here exist for `flash-bridge`'s run to call; until it does, `dead_code` is allowed
//! outside tests.
#![cfg_attr(not(test), allow(dead_code))]

use anyhow::{Result, bail};
use wartui_bridge::ports::PortCandidate;
use wartui_bridge::serial::Probed;
use wartui_bridge::spec::BridgeSpec;
use wartui_proto::mac::{self, Mac};

use super::{BoardInfo, Ran, Skip, matches, p4, parse_board_info};

/// The longest line kept; a longer run of bytes without a newline is dropped, as `boot.rs` does.
const MAX_LINE: usize = 1024;

/// What the loader prints before each of its lines.
const PREFIX: &str = "wartui-loader: ";

/// The silent CH343s flash-bridge may offer to set up, given what `target` refused: the one a
/// `--bridge` path names; else, with nothing named or remembered and no bridge answering, every
/// silent one; else none.
pub(super) fn blank_candidates(
    probed: &[Probed],
    named: Option<&BridgeSpec>,
    remembered: Option<Mac>,
) -> Vec<PortCandidate> {
    let silent = probed.iter().filter(|board| board.mac.is_none());
    match named {
        Some(spec @ BridgeSpec::Path(_)) => silent
            .filter(|board| matches(spec, &board.candidate))
            .map(|b| b.candidate.clone())
            .collect(),
        Some(_) => Vec::new(),
        None if remembered.is_some() || probed.iter().any(|board| board.mac.is_some()) => {
            Vec::new()
        }
        None => silent.map(|board| board.candidate.clone()).collect(),
    }
}

/// `board-info`'s answer, judged as a P4: Ok when it ran, read, and names `esp32p4`.
pub(super) fn judge_p4(probe: &Ran) -> Result<BoardInfo, Skip> {
    if !probe.ok {
        return Err(Skip::ProbeFailed(probe.last_line().to_owned()));
    }
    let info = parse_board_info(&probe.stdout).ok_or(Skip::Unreadable)?;
    if info.chip != "esp32p4" {
        return Err(Skip::OtherChip(info.chip));
    }
    Ok(info)
}

/// The one P4 to set up out of the judged candidates, or the refusal to print.
pub(super) fn choose(
    judged: Vec<(PortCandidate, Result<BoardInfo, Skip>)>,
) -> Result<(PortCandidate, BoardInfo)> {
    let mut p4s = Vec::new();
    let mut refused = Vec::new();
    for (board, result) in judged {
        match result {
            Ok(info) => p4s.push((board, info)),
            Err(skip) => refused.push(format!("{}: {skip}", board.path)),
        }
    }
    match p4s.len() {
        1 => Ok(p4s.remove(0)),
        0 => bail!(
            "no P4 bridge answered, and no silent CH343 is an ESP32-P4: {}. See {}",
            refused.join("; "),
            p4::FIRST_FLASH
        ),
        n => bail!(
            "{n} silent CH343s are ESP32-P4s: {}. Name the one to set up with --bridge PATH",
            p4s.iter().map(|(board, _)| board.path.as_str()).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// Whether the operator has agreed to the setup.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Consent {
    /// `--first-flash` said so.
    Given,
    /// Ask on the terminal.
    Ask,
    /// No flag and nobody to ask.
    Refused,
}

/// How consent is had, given `--first-flash` and whether stdin is a terminal.
pub(super) fn consent(first_flash: bool, terminal: bool) -> Consent {
    match (first_flash, terminal) {
        (true, _) => Consent::Given,
        (false, true) => Consent::Ask,
        (false, false) => Consent::Refused,
    }
}

/// The question put to the operator before a blank P4 is written.
pub(super) fn prompt_text(board: &PortCandidate, p4: &BoardInfo) -> String {
    format!(
        "{} is a blank ESP32-P4 ({}). Set it up as the bridge? This writes both chips, takes \
         about two minutes, and replaces any settings and Bluetooth bonds on it. The bridge is \
         known afterwards by its C6's address. [y/N] ",
        board.path,
        mac::short(&p4.mac)
    )
}

/// One line the loader prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Said {
    Start,
    /// The ESP-Hosted version on the C6, when it said one.
    Hosted(Option<String>),
    /// Percent of the C6 loader sent, in tens.
    Sending(u8),
    Sent,
    /// A step of the C6 loader's own progress, as worded.
    C6(String),
    Done,
    Failed(String),
    Skipped(String),
}

/// One whole line, trailing `\r` dropped, read as a loader line; None for anything else.
pub(super) fn said(line: &str) -> Option<Said> {
    let rest = line.trim_end_matches('\r').strip_prefix(PREFIX)?;
    Some(match rest {
        "start" => Said::Start,
        "sent" => Said::Sent,
        "done" => Said::Done,
        "hosted unknown" => Said::Hosted(None),
        _ => {
            if let Some(version) = rest.strip_prefix("hosted ") {
                Said::Hosted(Some(version.to_owned()))
            } else if let Some(percent) = rest.strip_prefix("sending ") {
                Said::Sending(percent.parse::<u8>().ok().filter(|&p| p <= 100)?)
            } else if let Some(step) = rest.strip_prefix("c6 ") {
                Said::C6(step.to_owned())
            } else if let Some(why) = rest.strip_prefix("failed ") {
                Said::Failed(why.to_owned())
            } else {
                Said::Skipped(rest.strip_prefix("skipped ")?.to_owned())
            }
        }
    })
}

/// Bytes from the CH343, joined into lines across reads; lines are capped at 1024 bytes as
/// `boot.rs` caps them.
pub(super) struct LoaderWatch {
    partial: Vec<u8>,
}

impl LoaderWatch {
    pub(super) fn new() -> Self {
        Self { partial: Vec::new() }
    }

    /// Take the next bytes read; the loader lines they complete.
    pub(super) fn feed(&mut self, bytes: &[u8]) -> Vec<Said> {
        self.partial.extend_from_slice(bytes);
        let mut found = Vec::new();
        while let Some(end) = self.partial.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=end).collect();
            found.extend(said(&String::from_utf8_lossy(&line[..end])));
        }
        if self.partial.len() >= MAX_LINE {
            self.partial.clear();
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use wartui_bridge::ports::PortCandidate;
    use wartui_bridge::serial::Probed;
    use wartui_bridge::spec::BridgeSpec;
    use wartui_proto::mac::Mac;

    use super::super::testing::{BOARD_INFO, BRIDGE_MAC, bridge, ch343, ran, silent};
    use super::super::{BoardInfo, Skip, p4, parse_board_info};
    use super::*;

    const P4_MAC: Mac = [0x02, 0x00, 0x5E, 0x10, 0x44, 0xC1];

    fn p4_transcript() -> String {
        BOARD_INFO
            .replace("esp32c5 (revision v0.1)", "esp32p4 (revision v3.2)")
            .replace("02:00:5e:10:a1:b0", "02:00:5e:10:44:c1")
    }

    fn p4_info() -> BoardInfo {
        parse_board_info(&p4_transcript()).unwrap()
    }

    fn paths(found: Vec<PortCandidate>) -> Vec<String> {
        found.into_iter().map(|c| c.path).collect()
    }

    #[test]
    fn setup_offers_silent_ch343s_when_nothing_is_remembered_or_named() {
        let probed = [silent("/dev/ttyACM0"), silent("/dev/ttyACM1")];
        let found = blank_candidates(&probed, None, None);
        assert_eq!(paths(found), ["/dev/ttyACM0", "/dev/ttyACM1"]);
    }

    #[test]
    fn setup_offers_named_path_when_its_ch343_is_silent() {
        let probed = [silent("/dev/ttyACM0"), silent("/dev/ttyACM1")];
        let named = BridgeSpec::Path("/dev/ttyACM0".into());
        let found = blank_candidates(&probed, Some(&named), None);
        assert_eq!(paths(found), ["/dev/ttyACM0"]);
    }

    #[test]
    fn setup_offers_nothing_when_remembered_bridge_is_silent() {
        let probed = [silent("/dev/ttyACM0")];
        assert!(blank_candidates(&probed, None, Some(BRIDGE_MAC)).is_empty());
    }

    #[test]
    fn setup_offers_nothing_when_named_by_address() {
        let probed = [silent("/dev/ttyACM0")];
        let named = BridgeSpec::Tail(vec![0x44, 0xC1]);
        assert!(blank_candidates(&probed, Some(&named), None).is_empty());
    }

    #[test]
    fn setup_offers_nothing_when_a_bridge_answered() {
        let probed: [Probed; 2] = [bridge("/dev/ttyACM0", &BRIDGE_MAC), silent("/dev/ttyACM1")];
        assert!(blank_candidates(&probed, None, None).is_empty());
    }

    #[test]
    fn setup_reads_board_as_p4_when_board_info_names_esp32p4() {
        let info = judge_p4(&ran(true, &p4_transcript())).unwrap();
        assert_eq!(info.chip, "esp32p4");
        assert_eq!(info.mac, P4_MAC);
    }

    #[test]
    fn setup_refuses_board_when_board_info_names_another_chip() {
        assert_eq!(judge_p4(&ran(true, BOARD_INFO)), Err(Skip::OtherChip("esp32c5".into())));
        assert_eq!(judge_p4(&ran(true, "")), Err(Skip::Unreadable));
        assert_eq!(judge_p4(&ran(false, "")), Err(Skip::ProbeFailed("no output".into())));
    }

    #[test]
    fn setup_chooses_only_p4_when_one_silent_ch343_is_a_p4() {
        let (a, b) = (ch343("/dev/ttyACM0"), ch343("/dev/ttyACM1"));
        let judged = vec![(a.clone(), Ok(p4_info())), (b, Err(Skip::ProbeFailed("busy".into())))];
        let (chosen, info) = choose(judged).unwrap();
        assert_eq!(chosen, a);
        assert_eq!(info.mac, P4_MAC);
    }

    #[test]
    fn setup_refuses_when_several_silent_ch343s_are_p4s() {
        let judged =
            vec![(ch343("/dev/ttyACM0"), Ok(p4_info())), (ch343("/dev/ttyACM1"), Ok(p4_info()))];
        let error = choose(judged).unwrap_err().to_string();
        assert!(error.contains("/dev/ttyACM0") && error.contains("/dev/ttyACM1"), "{error}");
        assert!(error.contains("--bridge"), "{error}");
    }

    #[test]
    fn setup_refuses_when_no_silent_ch343_is_a_p4() {
        // A port another program holds reads as a failed board-info, and says so.
        let a = ch343("/dev/ttyACM0");
        let error = choose(vec![(a, Err(Skip::ProbeFailed("port is busy".into())))])
            .unwrap_err()
            .to_string();
        assert!(error.contains("no P4 bridge answered"), "{error}");
        assert!(error.contains("/dev/ttyACM0") && error.contains("port is busy"), "{error}");
        assert!(error.contains(p4::FIRST_FLASH), "{error}");
    }

    #[test]
    fn setup_asks_when_on_a_terminal_without_first_flash() {
        assert_eq!(consent(false, true), Consent::Ask);
    }

    #[test]
    fn setup_takes_consent_when_first_flash_is_given() {
        assert_eq!(consent(true, false), Consent::Given);
        assert_eq!(consent(true, true), Consent::Given);
    }

    #[test]
    fn setup_refuses_when_no_terminal_and_no_first_flash() {
        assert_eq!(consent(false, false), Consent::Refused);
    }

    #[test]
    fn setup_words_prompt_when_offering_a_board() {
        let board = ch343("/dev/ttyACM0");
        let text = prompt_text(&board, &p4_info());
        for part in [
            "/dev/ttyACM0",
            "44:C1",
            "both chips",
            "about two minutes",
            "settings and Bluetooth bonds",
            "its C6's address",
            "[y/N]",
        ] {
            assert!(text.contains(part), "{part}: {text}");
        }
    }

    #[test]
    fn setup_reads_each_line_when_loader_reports_it() {
        assert_eq!(said("wartui-loader: start"), Some(Said::Start));
        assert_eq!(said("wartui-loader: hosted unknown"), Some(Said::Hosted(None)));
        assert_eq!(
            said("wartui-loader: hosted 2.12.13\r"),
            Some(Said::Hosted(Some("2.12.13".into())))
        );
        assert_eq!(said("wartui-loader: sending 40"), Some(Said::Sending(40)));
        assert_eq!(said("wartui-loader: sent"), Some(Said::Sent));
        assert_eq!(
            said("wartui-loader: c6 step writing the radio image"),
            Some(Said::C6("step writing the radio image".into()))
        );
        assert_eq!(said("wartui-loader: done"), Some(Said::Done));
        assert_eq!(
            said("wartui-loader: failed ota_begin ESP_FAIL"),
            Some(Said::Failed("ota_begin ESP_FAIL".into()))
        );
        assert_eq!(
            said("wartui-loader: skipped no ESP-Hosted on the C6"),
            Some(Said::Skipped("no ESP-Hosted on the C6".into()))
        );
    }

    #[test]
    fn setup_ignores_lines_when_they_are_idf_log_or_noise() {
        for line in [
            "\x1b[0;33mW (1234) H_SDIO_DRV: slave not ready\x1b[0m",
            "I (12) boot: ESP-IDF v5.5.4",
            "ESP-ROM:esp32p4-eco7-20260109",
            "wartui-loader: sending 140",
            "wartui-loader: bogus",
            "  wartui-loader: done",
        ] {
            assert_eq!(said(line), None, "{line:?}");
        }
    }

    #[test]
    fn setup_reads_loader_lines_when_split_across_reads() {
        let mut watch = LoaderWatch::new();
        assert_eq!(watch.feed(b"wartui-loader: do"), vec![]);
        assert_eq!(watch.feed(b"ne\r\nI (5) x\r\n"), vec![Said::Done]);
    }

    #[test]
    fn setup_caps_line_when_no_newline_arrives() {
        let mut watch = LoaderWatch::new();
        assert_eq!(watch.feed(&[b'x'; 2000]), vec![]);
        assert_eq!(watch.feed(b"\nwartui-loader: done\n"), vec![Said::Done]);
    }
}
