//! `flash-bridge`'s setup of a blank P4-WIFI6: choosing the board, asking the operator, writing
//! the P4 C6 loader, reading its verdict, then writing the bridge firmware and waiting for it.
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
//! `--bridge` path can, and it is offered whenever its own CH343 is silent.
//!
//! **Both images are had before the board is written**, so a failed build or download leaves
//! the board as it was. Both are merged, written at 0x0, and replace the board's settings.
//!
//! **espflash and the loader's reader never hold the port together.** They share the tty's
//! bytes, so a reader open during a write eats espflash's replies and espflash hangs. The reader
//! opens once the loader is written and closes before the bridge firmware is. The P4 loader waits
//! 2 s before its first line, which covers the open.
//!
//! **The loader's lines are read from the CH343 as they arrive.** They end in `\r\n` and sit
//! among ESP-IDF log lines that carry ANSI color, so a line counts only when it starts with
//! `wartui-loader: ` and parses; everything else is ignored. The watch gives up after 60 s with
//! no loader line, or 300 s in all. On the bench a rehearsal's push to the C6 ends 13 s in and
//! its verdict follows at 15 s; a real run adds the C6's own writes, about 650 KB, before it.
//!
//! **The bridge is known by its C6's address afterwards.** The running bridge reports its C6's
//! address, and its P4's own only while the C6 does not answer. So the setup succeeds only on a
//! `Ready` carrying another address than `board-info` read, and the `[bridge]` memory is written
//! from that one. A C6 that stays silent leaves nothing remembered, and a P4 that answers is
//! never set up, so a rerun updates it; retrying the setup means blanking the P4 first.

use std::io::{BufRead, ErrorKind, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use wartui_app::update::image::Chip;
use wartui_bridge::ports::PortCandidate;
use wartui_bridge::serial::Probed;
use wartui_bridge::spec::BridgeSpec;
use wartui_proto::mac::{self, Mac};

use super::bridge::{self, Args};
use super::{
    BoardInfo, Firmware, RELEASE_TAG, Ran, Skip, Source, check_image, espflash, espflash_present,
    fetch, flash_args, matches, obtain, p4, parse_board_info, probe_args, read, source,
};

/// The P4 C6 loader's merged image: a P4 app that lands the C6 loader through ESP-Hosted's OTA
/// (`firmware/p4-c6-loader/README.md`).
const P4_C6_LOADER: Firmware = Firmware {
    dir: concat!(env!("CARGO_MANIFEST_DIR"), "/../../firmware/p4-c6-loader"),
    // What `build.sh` leaves, merged already.
    elf: "build/wartui-p4-c6-loader-esp32p4.bin",
    what: "P4 C6 loader",
    chip: Chip::Esp32p4,
    partition_table: "partitions.csv",
    flash_size: Some("16mb"),
    merged: true,
    asset: "wartui-p4-c6-loader-esp32p4.bin",
};

/// The longest to wait for the next loader line.
const QUIET: Duration = Duration::from_secs(60);

/// The longest to watch the loader in all.
const WATCH_FOR: Duration = Duration::from_secs(300);

/// How long one read of the loader's port waits before the deadlines are checked again.
const READ_TIMEOUT: Duration = Duration::from_millis(100);

/// The longest to wait for the bridge firmware to answer with its C6's address.
const COME_UP: Duration = Duration::from_secs(90);

/// How long to wait between tries at the bridge firmware.
const COME_UP_RETRY: Duration = Duration::from_secs(2);

/// The longest line kept; a longer one is dropped whole, as `boot.rs` drops one.
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

/// The one P4 to set up out of the judged candidates, or the refusal to print. `judged` holds a
/// board at least: [`run`] is called only once [`blank_candidates`] has found one.
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
         about two minutes, and clears any settings and Bluetooth bonds on it. The bridge is \
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
    /// Percent of the C6 loader sent, 0 to 100.
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

/// Bytes from the CH343, joined into lines across reads. A line over 1024 bytes is dropped
/// whole, up to its newline, so its tail is never read as a line of its own.
pub(super) struct LoaderWatch {
    partial: Vec<u8>,
    /// The line being read is over the cap, and is dropped until its newline.
    dropping: bool,
}

impl LoaderWatch {
    pub(super) fn new() -> Self {
        Self { partial: Vec::new(), dropping: false }
    }

    /// Take the next bytes read; the loader lines they complete.
    pub(super) fn feed(&mut self, bytes: &[u8]) -> Vec<Said> {
        let mut found = Vec::new();
        for piece in bytes.split_inclusive(|&byte| byte == b'\n') {
            let (body, ends) = match piece.split_last() {
                Some((b'\n', body)) => (body, true),
                _ => (piece, false),
            };
            if !self.dropping {
                if self.partial.len() + body.len() > MAX_LINE {
                    self.partial.clear();
                    self.dropping = true;
                } else {
                    self.partial.extend_from_slice(body);
                }
            }
            if ends {
                if !self.dropping {
                    found.extend(said(&String::from_utf8_lossy(&self.partial)));
                }
                self.partial.clear();
                self.dropping = false;
            }
        }
        found
    }
}

/// How the loader ended, when the setup goes on.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// The C6 runs the bridge's radio firmware.
    Done,
    /// The C6 runs no ESP-Hosted, for this reason; it may already run the bridge's radio.
    Skipped(String),
}

/// Why the watch of the loader stopped short of a verdict the setup goes on from.
#[derive(Debug, PartialEq, Eq)]
enum Stopped {
    /// The loader's `failed` line: the text after `failed `.
    Failed(String),
    /// No loader line in this long.
    Quiet(Duration),
    /// No verdict in this long in all.
    Overdue(Duration),
    /// The port would not open, closed, or failed a read, as worded.
    Port(String),
}

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Stopped::Failed(why) => write!(f, "the loader failed: {why}"),
            Stopped::Quiet(quiet) => write!(f, "no line from the loader in {} s", quiet.as_secs()),
            Stopped::Overdue(total) => {
                write!(f, "no verdict from the loader in {} s", total.as_secs())
            }
            Stopped::Port(why) => f.write_str(why),
        }
    }
}

/// Read the loader's lines from `port` to its verdict, handing each to `say`. Gives up after
/// `quiet` with no loader line, or `total` in all; [`loader_failed_text`] words the `Err`.
fn watch(
    port: &mut dyn Read,
    quiet: Duration,
    total: Duration,
    mut say: impl FnMut(&Said),
) -> Result<Verdict, Stopped> {
    let started = Instant::now();
    let mut heard = started;
    let mut lines = LoaderWatch::new();
    let mut buffer = [0u8; 512];
    loop {
        let now = Instant::now();
        if now >= started + total {
            return Err(Stopped::Overdue(total));
        }
        if now >= heard + quiet {
            return Err(Stopped::Quiet(quiet));
        }
        let read = match port.read(&mut buffer) {
            // A hung-up tty reads EOF at once and forever.
            Ok(0) => return Err(Stopped::Port("its port closed".to_owned())),
            Ok(read) => read,
            Err(error) if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::Interrupted) => {
                continue;
            }
            Err(error) => return Err(Stopped::Port(format!("reading its port failed ({error})"))),
        };
        for said in lines.feed(&buffer[..read]) {
            heard = Instant::now();
            say(&said);
            match said {
                Said::Done => return Ok(Verdict::Done),
                Said::Skipped(why) => return Ok(Verdict::Skipped(why)),
                Said::Failed(why) => return Err(Stopped::Failed(why)),
                _ => {}
            }
        }
    }
}

/// What the C6 loader's first line is when it was built with `rehearse`.
const REHEARSAL: &str = "start rehearsal";

/// The line printed for each thing the loader says, by the P4's `label`.
struct Lines {
    label: String,
    /// The tenth of the push last printed.
    tenth: Option<u8>,
    /// The C6 loader said it is a rehearsal, so its `done` set nothing up.
    rehearsal: bool,
}

impl Lines {
    fn new(label: String) -> Self {
        Self { label, tenth: None, rehearsal: false }
    }

    /// The line to print for `said`, if any: none for `Skipped` and `Failed`, which the setup
    /// words, and the push's progress once per tenth.
    fn of(&mut self, said: &Said) -> Option<String> {
        let text = match said {
            Said::Start => "the loader is running".to_owned(),
            Said::Hosted(Some(version)) => format!("the C6 runs ESP-Hosted {version}"),
            Said::Hosted(None) => "the C6 runs ESP-Hosted, which names no version".to_owned(),
            Said::Sending(percent) => {
                let tenth = percent / 10;
                if self.tenth == Some(tenth) {
                    return None;
                }
                self.tenth = Some(tenth);
                format!("{percent:>3}%  sending the C6 loader")
            }
            Said::Sent => "the C6 loader is sent; the C6 runs it".to_owned(),
            Said::C6(text) => {
                self.rehearsal |= text == REHEARSAL;
                return Some(format!("{:<6} {text}", "C6"));
            }
            Said::Done if self.rehearsal => {
                "the C6 loader's rehearsal is done; writing the bridge firmware".to_owned()
            }
            Said::Done => "the C6 is set up; writing the bridge firmware".to_owned(),
            Said::Failed(_) | Said::Skipped(_) => return None,
        };
        Some(format!("{:<6} {text}", self.label))
    }
}

/// Set up the blank P4 among the `silent` CH343s: the loader, its verdict, the bridge firmware
/// and its C6's answer. `named` is whether `--bridge` named it, which keeps it unremembered.
pub(super) async fn run(args: &Args, named: bool, silent: Vec<PortCandidate>) -> Result<()> {
    refuse_image(args.image.as_deref())?;
    espflash_present()?;
    let judged = silent
        .into_iter()
        .map(|board| {
            let judged = judge_p4(&espflash(&probe_args(&board.path)));
            (board, judged)
        })
        .collect();
    let (board, info) = choose(judged)?;
    let path = board.path.clone();
    let label = mac::short(&info.mac).to_string();
    println!("found  {path}: an ESP32-P4 ({label}) whose bridge did not answer");
    let loader = source(&P4_C6_LOADER, args.loader.as_deref(), None, RELEASE_TAG);
    println!("loader {}", loader.describe(args.dry_run));
    let image = source(&p4::P4_BRIDGE, None, args.firmware_dir.as_deref(), RELEASE_TAG);
    println!("image  {}", image.describe(args.dry_run));
    if args.dry_run {
        if let Source::File(file) = &loader {
            check_image(&read(file)?, Chip::Esp32p4, true)
                .with_context(|| format!("checking {}", file.display()))?;
        }
        println!("{path} would be set up: the loader, then the bridge firmware");
        return Ok(());
    }
    match consent(args.first_flash, std::io::stdin().is_terminal()) {
        Consent::Given => {}
        Consent::Ask => {
            print!("{}", prompt_text(&board, &info));
            std::io::stdout().flush().context("asking on the terminal")?;
            let mut answer = String::new();
            std::io::stdin().lock().read_line(&mut answer).context("reading the answer")?;
            if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                println!("{path} not set up");
                return Ok(());
            }
        }
        Consent::Refused => bail!(
            "standard input is not a terminal, so nothing can answer; pass --first-flash to set \
             up {path}"
        ),
    }

    let loader = match &loader {
        Source::Build(dir) => build_loader(dir)?,
        other => fetch(&P4_C6_LOADER, other)?,
    };
    check_image(&read(&loader)?, Chip::Esp32p4, true)
        .with_context(|| format!("checking {}", loader.display()))?;
    let image = obtain(&p4::P4_BRIDGE, &image)?;

    println!("{label:<6} writing the loader");
    write(&path, &loader)
        .map_err(|why| anyhow::anyhow!("writing the loader to {path} failed: {why}"))?;
    let verdict = {
        let (path, label) = (path.clone(), label.clone());
        tokio::task::spawn_blocking(move || {
            let mut port = wartui_desktop::serial::open_reader(&path, 115_200, READ_TIMEOUT)
                .map_err(|error| Stopped::Port(format!("its port would not open ({error})")))?;
            let mut lines = Lines::new(label);
            watch(port.as_mut(), QUIET, WATCH_FOR, |said| {
                if let Some(line) = lines.of(said) {
                    println!("{line}");
                }
            })
            // The reader closes here, before espflash opens the port again.
        })
        .await
        .context("watching the loader")?
    };
    match verdict {
        Ok(Verdict::Done) => {}
        Ok(Verdict::Skipped(why)) => {
            println!("{path}  the C6 loader was skipped: {why}; writing the bridge firmware");
        }
        Err(stopped) => bail!("{}", loader_failed_text(&path, &stopped)),
    }
    write(&path, &image)
        .map_err(|why| anyhow::anyhow!("writing the bridge firmware to {path} failed: {why}"))?;

    println!("{path}  waiting for the bridge to answer with its C6's address");
    let Some(c6) = crate::interruptible(come_up(&label, &path, info.mac)).await? else {
        bail!("{}", c6_silent_text(&path));
    };
    println!("{:<6} set up; the bridge is known by its C6's address", mac::short(&c6));
    bridge::remember(args, named, &board, c6);
    Ok(())
}

/// Write the merged `image` to the P4 on `path`; the `Err` is espflash's last line.
fn write(path: &str, image: &Path) -> Result<(), String> {
    let ran = espflash(&flash_args(path, Chip::Esp32p4, image));
    if ran.ok { Ok(()) } else { Err(ran.last_line().to_owned()) }
}

/// Refuse `--image` in a setup: it is app-only, and the board needs a merged image at 0x0.
fn refuse_image(image: Option<&Path>) -> Result<()> {
    ensure!(
        image.is_none(),
        "--image is an app-only image, and a new board needs the bridge's merged image; drop \
         --image, or use --firmware-dir"
    );
    Ok(())
}

/// Wait up to [`COME_UP`] in all for the bridge firmware on `path` to answer with an address
/// other than its P4's, `own`: its C6's. `None` when it never does.
async fn come_up(label: &str, path: &str, own: Mac) -> Result<Option<Mac>> {
    let deadline = tokio::time::Instant::now() + COME_UP;
    loop {
        // Each try ends at the deadline, whatever its own wait for a `Ready`.
        let tried = tokio::time::timeout_at(deadline, p4::open_usb(label, path)).await;
        match tried {
            Err(_) => return Ok(None),
            // Its own address is a C6 not answering yet.
            Ok(Ok((link, core))) => {
                link.close().await;
                if core.mac != own {
                    return Ok(Some(core.mac));
                }
            }
            // A P4 still starting.
            Ok(Err(_)) => {}
        }
        if tokio::time::Instant::now() + COME_UP_RETRY >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(COME_UP_RETRY).await;
    }
}

/// Build the P4 C6 loader in `dir` with its `build.sh`, which needs ESP-IDF exported.
fn build_loader(dir: &Path) -> Result<PathBuf> {
    match Command::new("idf.py").arg("--version").stdin(Stdio::null()).output() {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => bail!(
            "idf.py is not on PATH, so the P4 C6 loader cannot be built; export ESP-IDF v5.5.4 \
             or pass --loader BIN"
        ),
        Err(error) => return Err(error).context("running idf.py"),
    }
    let mut build = Command::new(dir.join("build.sh"));
    // The C6 loader's own `rust-toolchain.toml` and target directory decide where it builds, and
    // these would override them (`super::build_image`). A setup writes the real C6 loader, never
    // a rehearsal a shell left `C6_LOADER_FEATURES` set for.
    build
        .current_dir(dir)
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("C6_LOADER_FEATURES");
    let status = build.status().context("running build.sh")?;
    ensure!(status.success(), "building the P4 C6 loader in {} failed", dir.display());
    Ok(dir.join(P4_C6_LOADER.elf))
}

/// What the P4 loader's `failed` line carries when the C6 loader failed.
const C6_FAILED: &str = "c6 loader: ";

/// Why the setup of `path` stopped while the loader ran, and what to do.
fn loader_failed_text(path: &str, stopped: &Stopped) -> String {
    let next = match stopped {
        Stopped::Failed(why) if why.starts_with(C6_FAILED) => "The C6 loader refused or failed",
        Stopped::Quiet(_) | Stopped::Overdue(_) => {
            "The P4 runs the loader, and running flash-bridge again retries. The C6 loader may \
             still be writing, so wait a minute before running flash-bridge again"
        }
        Stopped::Failed(_) | Stopped::Port(_) => {
            "The P4 runs the loader, and running flash-bridge again retries"
        }
    };
    format!(
        "setting up {path} stopped: {stopped}. {next}. The C6's header is the fallback, per {}",
        p4::FIRST_FLASH
    )
}

/// Why the setup of `path` ended without its C6 answering. The P4 then answers with its own
/// address, so a plain rerun takes the ordinary update rather than the setup.
fn c6_silent_text(path: &str) -> String {
    format!(
        "{path} runs the bridge firmware, but its C6 is not answering, so the bridge is not \
         remembered. Running flash-bridge again updates the P4 and does not retry the setup. To \
         retry it, blank the P4 with `espflash erase-region --chip esp32p4 --port {path} 0x0 \
         0x10000`, then run flash-bridge --bridge {path} --first-flash. The C6's header is the \
         fallback, per {}",
        p4::FIRST_FLASH
    )
}

#[cfg(test)]
mod tests {
    use wartui_bridge::ports::PortCandidate;
    use wartui_bridge::serial::Probed;
    use wartui_bridge::spec::BridgeSpec;
    use wartui_proto::mac::Mac;

    use super::super::testing::{BOARD_INFO, BRIDGE_MAC, bridge, ch343, p4_app_image, ran, silent};
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
        assert_eq!(
            said("wartui-loader: c6 start rehearsal\r"),
            Some(Said::C6("start rehearsal".into()))
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
        // The overlong line is dropped whole, its tail included, up to its own newline.
        let mut watch = LoaderWatch::new();
        assert_eq!(watch.feed(&[b'x'; 2000]), vec![]);
        assert_eq!(watch.feed(b"wartui-loader: done\n"), vec![]);
        assert_eq!(watch.feed(b"wartui-loader: done\n"), vec![Said::Done]);
        // Within one read too.
        let mut long = b"wartui-loader: c6 ".to_vec();
        long.extend_from_slice(&[b'y'; 2000]);
        long.extend_from_slice(b"\nwartui-loader: sent\n");
        assert_eq!(watch.feed(&long), vec![Said::Sent]);
    }

    #[test]
    fn setup_offers_nothing_when_named_path_matches_no_silent_board() {
        let probed = [silent("/dev/ttyACM0")];
        let named = BridgeSpec::Path("/dev/ttyACM1".into());
        assert!(blank_candidates(&probed, Some(&named), None).is_empty());
    }

    #[test]
    fn setup_offers_nothing_when_named_path_board_answered() {
        let probed = [bridge("/dev/ttyACM0", &BRIDGE_MAC), silent("/dev/ttyACM1")];
        let named = BridgeSpec::Path("/dev/ttyACM0".into());
        assert!(blank_candidates(&probed, Some(&named), None).is_empty());
    }

    #[test]
    fn setup_says_rerun_and_header_when_loader_fails() {
        let stopped = Stopped::Failed("no verdict from the C6 loader in 120 s".into());
        let text = loader_failed_text("/dev/ttyACM0", &stopped);
        assert!(
            text.contains("running flash-bridge again retries") && text.contains(p4::FIRST_FLASH),
            "{text}"
        );
        assert!(!text.contains("wait a minute"), "{text}");
    }

    #[test]
    fn setup_says_header_and_no_rerun_when_c6_loader_fails() {
        let stopped = Stopped::Failed("c6 loader: no slot boots under this table".into());
        let text = loader_failed_text("/dev/ttyACM0", &stopped);
        assert!(text.contains("The C6 loader refused or failed"), "{text}");
        assert!(text.contains(p4::FIRST_FLASH), "{text}");
        assert!(!text.contains("again retries"), "{text}");
    }

    #[test]
    fn setup_says_wait_a_minute_when_loader_falls_quiet() {
        for stopped in [Stopped::Quiet(QUIET), Stopped::Overdue(WATCH_FOR)] {
            let text = loader_failed_text("/dev/ttyACM0", &stopped);
            assert!(text.contains(&stopped.to_string()), "{text}");
            assert!(
                text.contains("may still be writing, so wait a minute before running flash-bridge"),
                "{text}"
            );
            assert!(text.contains(p4::FIRST_FLASH), "{text}");
        }
    }

    #[test]
    fn setup_says_rerun_and_header_when_port_closes() {
        let text = loader_failed_text("/dev/ttyACM0", &Stopped::Port("its port closed".into()));
        assert!(text.contains("stopped: its port closed. The P4 runs the loader"), "{text}");
        assert!(text.contains("again retries") && !text.contains("wait a minute"), "{text}");
    }

    #[test]
    fn setup_says_how_to_retry_when_c6_stays_silent() {
        let text = c6_silent_text("/dev/ttyACM0");
        assert!(text.contains("not remembered") && text.contains(p4::FIRST_FLASH), "{text}");
        assert!(text.contains("does not retry the setup"), "{text}");
        assert!(
            text.contains("espflash erase-region --chip esp32p4 --port /dev/ttyACM0 0x0 0x10000"),
            "{text}"
        );
        assert!(text.contains("--bridge /dev/ttyACM0 --first-flash"), "{text}");
    }

    #[test]
    fn setup_fetches_release_assets_when_built_for_a_release() {
        assert_eq!(
            source(&P4_C6_LOADER, None, None, Some("v1")),
            Source::Release { tag: "v1".into(), asset: "wartui-p4-c6-loader-esp32p4.bin" }
        );
        assert_eq!(
            source(&p4::P4_BRIDGE, None, None, Some("v1")),
            Source::Release { tag: "v1".into(), asset: "wartui-p4-bridge-fw-esp32p4.bin" }
        );
        assert_eq!(
            source(&P4_C6_LOADER, Some(Path::new("l.bin")), None, Some("v1")),
            Source::File("l.bin".into())
        );
    }

    #[test]
    fn setup_names_assets_when_release_yml_publishes_them() {
        let workflow = include_str!("../../../../.github/workflows/release.yml");
        for asset in [P4_C6_LOADER.asset, p4::P4_BRIDGE.asset] {
            assert!(workflow.contains(asset), "{asset}");
        }
    }

    #[test]
    fn setup_refuses_image_when_setting_up_a_board() {
        // An app-only image cannot be written at 0x0, and dropping it silently would set the
        // board up from another source than the one named.
        let refused = refuse_image(Some(Path::new("fw.bin"))).unwrap_err().to_string();
        assert_eq!(
            refused,
            "--image is an app-only image, and a new board needs the bridge's merged image; \
             drop --image, or use --firmware-dir"
        );
        assert!(refuse_image(None).is_ok());
    }

    #[test]
    fn setup_refuses_loader_file_when_it_is_app_only() {
        assert!(check_image(&p4_app_image(), Chip::Esp32p4, true).is_err());
    }

    #[test]
    fn setup_goes_on_when_loader_says_done() {
        let mut port: &[u8] = b"I (5) x\r\nwartui-loader: start\r\nwartui-loader: done\r\n";
        let mut heard = Vec::new();
        let verdict = watch(&mut port, QUIET, WATCH_FOR, |said| heard.push(said.clone()));
        assert_eq!(verdict, Ok(Verdict::Done));
        assert_eq!(heard, [Said::Start, Said::Done]);
    }

    #[test]
    fn setup_goes_on_when_loader_skips() {
        let mut port: &[u8] = b"wartui-loader: skipped no ESP-Hosted on the C6\r\n";
        let verdict = watch(&mut port, QUIET, WATCH_FOR, |_| {});
        assert_eq!(verdict, Ok(Verdict::Skipped("no ESP-Hosted on the C6".into())));
    }

    #[test]
    fn setup_stops_when_loader_fails() {
        let mut port: &[u8] = b"wartui-loader: failed ota_begin: ESP_FAIL\r\n";
        let why = watch(&mut port, QUIET, WATCH_FOR, |_| {}).unwrap_err();
        assert_eq!(why, Stopped::Failed("ota_begin: ESP_FAIL".into()));
    }

    #[test]
    fn setup_stops_when_port_closes_before_a_verdict() {
        let mut port: &[u8] = b"wartui-loader: start\r\nwartui-loader: sending 10\r\n";
        let why = watch(&mut port, QUIET, WATCH_FOR, |_| {}).unwrap_err();
        assert!(why.to_string().contains("closed"), "{why}");
    }

    /// A port that never has anything to read.
    struct Quiet;

    impl std::io::Read for Quiet {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::TimedOut.into())
        }
    }

    #[test]
    fn setup_stops_when_loader_falls_quiet() {
        let why = watch(&mut Quiet, Duration::ZERO, WATCH_FOR, |_| {}).unwrap_err();
        assert!(why.to_string().contains("no line from the loader"), "{why}");
        let why = watch(&mut Quiet, QUIET, Duration::ZERO, |_| {}).unwrap_err();
        assert!(why.to_string().contains("no verdict from the loader"), "{why}");
    }

    #[test]
    fn setup_prints_sending_when_a_tenth_is_reached() {
        let mut lines = Lines::new("44:C1".into());
        let printed: Vec<String> = [0, 4, 9, 10, 11, 25, 100]
            .into_iter()
            .filter_map(|p| lines.of(&Said::Sending(p)))
            .collect();
        assert_eq!(printed.len(), 4, "{printed:?}");
        assert!(printed[3].contains("100%"), "{printed:?}");
        let c6 = lines.of(&Said::C6("step writing the radio image".into())).unwrap();
        assert_eq!(c6, "C6     step writing the radio image");
    }

    #[test]
    fn setup_says_rehearsal_is_done_when_c6_loader_started_a_rehearsal() {
        let mut lines = Lines::new("44:C1".into());
        let done = lines.of(&Said::Done).unwrap();
        assert!(done.contains("the C6 is set up"), "{done}");
        let mut lines = Lines::new("44:C1".into());
        assert_eq!(
            lines.of(&Said::C6("start rehearsal".into())).unwrap(),
            "C6     start rehearsal"
        );
        let done = lines.of(&Said::Done).unwrap();
        assert!(done.contains("rehearsal is done") && !done.contains("set up"), "{done}");
    }
}
