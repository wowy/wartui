//! `wartui ota` — update nodes over the air, one at a time, through the bridge.
//!
//! **It listens before it acts.** A node is only reachable in the admin window its heartbeat
//! opens, and only a heartbeat says what board and version it is, so the run gathers them for
//! `--listen` seconds first: long enough for an assigned node, which beats every 5 s, to be
//! heard once. Only the heartbeat's frozen prefix is read, so a node a minor version behind,
//! which `run` cannot drive, is exactly the node this reaches.
//!
//! **The table is the decision, and it is shown before anything is sent.** Each node's action
//! follows from its version and whether there is an image for its board. A node on trial is
//! updated like any other: it confirms its running image on the first update frame it decodes,
//! before taking the update into its other slot.
//!
//! **One node at a time.** The session keeps one chunk in flight, so two would share the
//! bridge's time anyway, and a fleet that fails one at a time fails with every other node
//! still on its old image. Frames from the other nodes are ignored meanwhile.
//!
//! **Events that queued while the run was busy are thrown away** before each update starts: a
//! heartbeat that waited out an image build or the prompt is no admin window, and a `begin`
//! sent on one goes nowhere. [`wartui_core::ota`] has the session's own reasoning.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use wartui_bridge::serial::BridgeSpec;
use wartui_bridge::sim::{SimConfig, SimTransport, minor_behind};
use wartui_bridge::{LinkEvent, LinkHandle};
use wartui_core::ota::{OtaImage, OtaSession, Outcome, Phase, Progress};
use wartui_core::runtime::{drive_ota, now};
use wartui_proto::air::{Board, Capabilities, NODE_VERSION, Version};
use wartui_proto::link::BridgeToHost;
use wartui_proto::mac::{self, Mac};

use crate::flash::app::{self, AppImage, board_name};

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

    /// Update this node, by address or the end of it (`A1:B0`). Repeatable. Every node heard
    /// by default.
    #[arg(long = "node", value_name = "MAC", value_parser = node_arg)]
    nodes: Vec<BridgeSpec>,

    /// Send this app-only image, to the nodes of the board it was built for, rather than
    /// building or downloading one per board.
    #[arg(long, value_name = "BIN", conflicts_with = "firmware_dir")]
    image: Option<PathBuf>,

    /// Build each board's image from the node firmware in this directory.
    #[arg(long, value_name = "DIR")]
    firmware_dir: Option<PathBuf>,

    /// How long to listen for heartbeats before choosing, in seconds.
    #[arg(long, value_name = "SECS", default_value_t = 6)]
    listen: u64,

    /// Also update a node already on the image's version, or on a newer one.
    #[arg(long)]
    force: bool,

    /// Update without asking.
    #[arg(long, short)]
    yes: bool,

    /// Update a simulated fleet instead of hardware. Its nodes run an older version, so there
    /// is something to update.
    #[arg(long, value_name = "NODES", num_args = 0..=1, default_missing_value = "3")]
    sim: Option<u8>,
}

/// A `--node` value: an address, or the end of one.
fn node_arg(text: &str) -> Result<BridgeSpec, String> {
    match text.parse::<BridgeSpec>() {
        Ok(BridgeSpec::Path(_)) | Err(_) => {
            Err(format!("'{text}' is not an address like 3C:DC:75:84:A1:B0, or the end of one"))
        }
        Ok(spec) => Ok(spec),
    }
}

/// What the run does with one node.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    Update,
    /// On trial on the image's version already: confirm it rather than send it again.
    Confirm,
    /// Already on the image's version.
    Same(Version),
    /// On a newer version than the image.
    Older,
    /// No image was found or named for its board.
    NoImage(Board),
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Update => f.write_str("update"),
            Self::Confirm => f.write_str("confirm"),
            Self::Same(version) => write!(f, "skip: already {version}"),
            Self::Older => f.write_str("skip: older image (use --force)"),
            Self::NoImage(board) => write!(f, "refuse: no image for {}", board_name(*board)),
        }
    }
}

/// Decide what to do with a node announcing `node`, given the image for its board.
fn decide(node: &Capabilities, image: Option<Version>, force: bool) -> Action {
    let Some(image) = image else { return Action::NoImage(node.board) };
    if node.trial && image == node.version {
        return Action::Confirm;
    }
    match image.cmp(&node.version) {
        _ if force => Action::Update,
        std::cmp::Ordering::Greater => Action::Update,
        std::cmp::Ordering::Equal => Action::Same(image),
        std::cmp::Ordering::Less => Action::Older,
    }
}

pub async fn run(args: Args) -> Result<()> {
    if !args.yes && !std::io::stdin().is_terminal() {
        bail!("standard input is not a terminal, so nothing can answer; pass --yes to update");
    }
    let mut link = open(&args)?;
    let info = match tokio::time::timeout(super::CONNECT_NOTICE_AFTER, connected(&mut link)).await {
        Ok(info) => info?,
        Err(_) => bail!("{}", super::no_bridge_notice(args.bridge.as_deref())),
    };
    println!("bridge  {} on {:?}, firmware {}", mac::full(&info.mac), info.chip, info.fw_version);
    println!("listening {} s for heartbeats", args.listen);
    let heard = listen(&mut link, Duration::from_secs(args.listen)).await?;
    let targets = select(&heard, &args.nodes)?;

    let images = images(&args, &targets).await?;
    let plan: Vec<(Mac, Capabilities, Option<Version>, Action)> = targets
        .iter()
        .map(|(node, caps)| {
            let image = images.get(&caps.board).map(|image| image.version);
            (*node, *caps, image, decide(caps, image, args.force))
        })
        .collect();
    print_table(&plan);

    let chosen: Vec<(Mac, Capabilities, bool)> = plan
        .iter()
        .filter(|(.., action)| matches!(action, Action::Update | Action::Confirm))
        .map(|(node, caps, _, action)| (*node, *caps, *action == Action::Confirm))
        .collect();
    if chosen.is_empty() {
        println!("nothing to update");
        return Ok(());
    }
    if !args.yes && !tokio::task::spawn_blocking(confirm).await.context("asking")?? {
        println!("nothing updated");
        return Ok(());
    }

    let mut failed = 0;
    // From the clock, so a host restarted mid-update is unlikely to reuse the byte; the node
    // copes if it does.
    let base = u8::try_from(chrono::Utc::now().timestamp().rem_euclid(256)).unwrap_or(1);
    let mut next_id = 1;
    let mut terminate = crate::Terminate::new();
    for (i, (node, caps, confirm_only)) in chosen.iter().enumerate() {
        drain(&mut link).await;
        let session = base.wrapping_add(u8::try_from(i).unwrap_or(0));
        let mut ota = if *confirm_only {
            OtaSession::confirm_trial(*node, caps.version, next_id, now())
        } else {
            let image = &images[&caps.board].ota;
            OtaSession::new(*node, caps.version, image, session, next_id, now())
        };
        let mut line = ProgressLine::new(*node);
        let stop = async {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                () = terminate.recv() => {}
            }
        };
        let outcome = drive_ota(&mut link, &mut ota, stop, |p| line.show(p)).await;
        next_id = ota.next_id();
        line.finish(&outcome);
        match outcome {
            Outcome::Done(_) => {}
            Outcome::Failed(wartui_core::ota::Failure::Cancelled) => bail!("cancelled"),
            Outcome::Failed(_) => failed += 1,
        }
    }
    if failed > 0 {
        bail!("{failed} of {} nodes failed to update", chosen.len());
    }
    Ok(())
}

fn open(args: &Args) -> Result<LinkHandle> {
    if let Some(node_count) = args.sim {
        let config = SimConfig {
            node_count,
            fleet_version: minor_behind(NODE_VERSION),
            ..SimConfig::default()
        };
        return SimTransport::new(config).start().context("starting the simulator");
    }
    let bridge = args.bridge.as_deref();
    super::open(bridge, None, 0, super::memory(args.config.as_deref()))
}

async fn connected(link: &mut LinkHandle) -> Result<wartui_bridge::BridgeInfo> {
    loop {
        match link.recv().await {
            Some(LinkEvent::Connected(info)) => return Ok(info),
            Some(LinkEvent::Disconnected { reason }) => bail!("{reason}"),
            Some(_) => {}
            None => bail!("the link closed"),
        }
    }
}

/// Every node heartbeating within `period`, as its latest heartbeat's prefix says.
async fn listen(link: &mut LinkHandle, period: Duration) -> Result<BTreeMap<Mac, Capabilities>> {
    let mut heard = BTreeMap::new();
    let deadline = tokio::time::Instant::now() + period;
    loop {
        let event = match tokio::time::timeout_at(deadline, link.recv()).await {
            Err(_) => return Ok(heard),
            Ok(None) => bail!("the link closed"),
            Ok(Some(event)) => event,
        };
        if let LinkEvent::Message(BridgeToHost::Rx { src, payload, .. }) = event
            && let Ok(caps) = Capabilities::decode_prefix(&payload)
        {
            heard.insert(src, caps);
        }
    }
}

/// Throw away whatever queued while the run was doing something else.
async fn drain(link: &mut LinkHandle) {
    while let Ok(Some(_)) = tokio::time::timeout(Duration::ZERO, link.recv()).await {}
}

/// The nodes `--node` names among those heard, or every one heard.
fn select(
    heard: &BTreeMap<Mac, Capabilities>,
    specs: &[BridgeSpec],
) -> Result<Vec<(Mac, Capabilities)>> {
    if heard.is_empty() {
        bail!("no node heartbeated; check they are powered and in range of the bridge");
    }
    if specs.is_empty() {
        return Ok(heard.iter().map(|(node, caps)| (*node, *caps)).collect());
    }
    let mut chosen = BTreeMap::new();
    for spec in specs {
        let matching: Vec<(&Mac, &Capabilities)> =
            heard.iter().filter(|(node, _)| spec.matches_address(node)).collect();
        match matching.as_slice() {
            [] => bail!("no node heard is {spec}"),
            [(node, caps)] => {
                chosen.insert(**node, **caps);
            }
            several => {
                let names: Vec<String> =
                    several.iter().map(|(node, _)| mac::full(node).to_string()).collect();
                bail!(
                    "several nodes heard end with {spec} ({}); name one by more of its address",
                    names.join(", ")
                );
            }
        }
    }
    Ok(chosen.into_iter().collect())
}

/// An image ready to send.
struct Ready {
    version: Version,
    ota: OtaImage,
}

/// One image per board among `targets`: `--image` for its own board, else a build or a
/// release download per board.
async fn images(args: &Args, targets: &[(Mac, Capabilities)]) -> Result<HashMap<Board, Ready>> {
    let mut boards: Vec<Board> = targets.iter().map(|(_, caps)| caps.board).collect();
    boards.sort_by_key(|board| board.as_u8());
    boards.dedup();
    let image = args.image.clone();
    let firmware_dir = args.firmware_dir.clone();
    // Builds and downloads block, and the link's reader must keep running meanwhile.
    let found = tokio::task::spawn_blocking(move || -> Result<Vec<AppImage>> {
        if let Some(path) = image {
            let image = app::load(&path)?;
            println!(
                "image   {} for {}, {}",
                image.version,
                board_name(image.board),
                path.display()
            );
            return Ok(vec![image]);
        }
        let mut found = Vec::new();
        for board in boards {
            if matches!(board, Board::Unknown(_)) {
                continue;
            }
            let source = app::source_for(board, firmware_dir.as_deref())?;
            println!("image   {}: {}", board_name(board), source.describe(false));
            let image = app::obtain(board, &source)?;
            println!("image   {} for {}", image.version, board_name(board));
            found.push(image);
        }
        Ok(found)
    })
    .await
    .context("finding images")??;
    Ok(found
        .into_iter()
        .map(|image| {
            let ota = OtaImage {
                bytes: Arc::from(image.bytes),
                sha256: image.sha256,
                version: image.version,
                board: image.board,
            };
            (image.board, Ready { version: image.version, ota })
        })
        .collect())
}

fn print_table(plan: &[(Mac, Capabilities, Option<Version>, Action)]) {
    println!();
    println!("{:<7}  {:<14}  {:<8}  {:<8}  action", "node", "board", "version", "image");
    for (node, caps, image, action) in plan {
        let image = image.map_or_else(|| "—".to_owned(), |v| v.to_string());
        let version =
            if caps.trial { format!("{};trial", caps.version) } else { caps.version.to_string() };
        println!(
            "{:<7}  {:<14}  {:<8}  {:<8}  {action}",
            mac::short(node).to_string(),
            board_name(caps.board),
            version,
            image
        );
    }
    println!();
}

/// Ask on the terminal. `run` has already refused when standard input is not one.
fn confirm() -> Result<bool> {
    eprint!("Update these nodes? [y/N] ");
    std::io::stderr().flush().context("writing the question")?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer).context("reading the answer")?;
    Ok(matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

/// One node's progress, redrawn in place on a terminal and a line per phase otherwise.
struct ProgressLine {
    node: String,
    tty: bool,
    phase: Option<Phase>,
    drawn: Option<Instant>,
    /// When streaming started, and how much the node held then, for the rate.
    streaming: Option<(Instant, u32)>,
    /// When the node first held the whole image, which stops the rate's clock.
    streamed: Option<Instant>,
}

impl ProgressLine {
    fn new(node: Mac) -> Self {
        Self {
            node: mac::short(&node).to_string(),
            tty: std::io::stdout().is_terminal(),
            phase: None,
            drawn: None,
            streaming: None,
            streamed: None,
        }
    }

    fn show(&mut self, progress: &Progress) {
        let now = Instant::now();
        if progress.phase == Phase::Streaming && self.streaming.is_none() {
            self.streaming = Some((now, progress.acked));
        }
        if progress.acked == progress.total && self.streamed.is_none() {
            self.streamed = Some(now);
        }
        // `finish` says how it ended.
        if matches!(progress.phase, Phase::Done | Phase::Failed) {
            return;
        }
        let changed = self.phase != Some(progress.phase);
        let due = self.drawn.is_none_or(|at| now.duration_since(at) >= Duration::from_millis(250));
        if !(changed || self.tty && due) {
            return;
        }
        self.phase = Some(progress.phase);
        self.drawn = Some(now);
        let text = self.render(progress, now);
        if self.tty {
            print!("\r\x1b[K{text}");
            let _ = std::io::stdout().flush();
        } else {
            println!("{text}");
        }
    }

    fn render(&self, progress: &Progress, now: Instant) -> String {
        // A confirm alone sends no image, and is all of what is left to do.
        let percent = if progress.total == 0 {
            100
        } else {
            u64::from(progress.acked) * 100 / u64::from(progress.total)
        };
        let rate = self.streaming.map_or(0, |(since, from)| {
            let secs = self.streamed.unwrap_or(now).duration_since(since).as_secs_f64();
            let bytes = f64::from(progress.acked.saturating_sub(from));
            // Whole KB/s, from at most a slot's worth of bytes.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let rate = if secs > 0.0 { (bytes / secs / 1024.0) as u64 } else { 0 };
            rate
        });
        format!("{:<7}  {percent:>3}%  {rate:>4} KB/s  {}", self.node, progress.phase)
    }

    fn finish(&self, outcome: &Outcome) {
        let result = match outcome {
            Outcome::Done(version) => format!("done {version} (confirmed)"),
            Outcome::Failed(failure) => format!("failed: {failure}"),
        };
        if self.tty {
            print!("\r\x1b[K");
        }
        println!("{:<7}  {result}", self.node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEW: Version = Version::new(0, 1, 1);

    fn caps(version: Version, trial: bool) -> Capabilities {
        Capabilities { version, trial, ..Capabilities::here(Board::Esp32c6) }
    }

    #[test]
    fn ota_cmd_updates_node_when_image_is_newer() {
        let node = caps(Version::new(0, 0, 9), false);
        assert_eq!(decide(&node, Some(NEW), false), Action::Update);
        assert_eq!(decide(&node, Some(NEW), false).to_string(), "update");
    }

    #[test]
    fn ota_cmd_skips_node_when_already_on_image_version() {
        let action = decide(&caps(NEW, false), Some(NEW), false);
        assert_eq!(action.to_string(), "skip: already 0.1.1");
        assert_eq!(decide(&caps(NEW, false), Some(NEW), true), Action::Update);
    }

    #[test]
    fn ota_cmd_skips_node_when_image_is_older() {
        let action = decide(&caps(Version::new(0, 2, 0), false), Some(NEW), false);
        assert_eq!(action.to_string(), "skip: older image (use --force)");
        assert_eq!(decide(&caps(Version::new(0, 2, 0), false), Some(NEW), true), Action::Update);
    }

    #[test]
    fn ota_cmd_refuses_node_when_no_image_for_board() {
        let action = decide(&caps(Version::new(0, 0, 9), false), None, true);
        assert_eq!(action.to_string(), "refuse: no image for esp32c6");
    }

    #[test]
    fn ota_cmd_confirms_node_when_it_runs_the_image_version_on_trial() {
        let action = decide(&caps(NEW, true), Some(NEW), false);
        assert_eq!(action, Action::Confirm);
        assert_eq!(action.to_string(), "confirm");
        // Not sent again, even when forced.
        assert_eq!(decide(&caps(NEW, true), Some(NEW), true), Action::Confirm);
    }

    #[test]
    fn ota_cmd_updates_node_when_it_runs_a_trial_image() {
        // The node confirms its running image on the update's first frame.
        let action = decide(&caps(Version::new(0, 0, 9), true), Some(NEW), false);
        assert_eq!(action, Action::Update);
    }

    #[test]
    fn ota_cmd_selects_nodes_by_address_tail_when_named() {
        let a = [0x3C, 0xDC, 0x75, 0x84, 0xA1, 0xB0];
        let b = [0x3C, 0xDC, 0x75, 0x84, 0x57, 0xB0];
        let heard: BTreeMap<Mac, Capabilities> =
            [(a, caps(NEW, false)), (b, caps(NEW, false))].into();
        assert_eq!(select(&heard, &[]).unwrap().len(), 2);
        let chosen = select(&heard, &[node_arg("A1:B0").unwrap()]).unwrap();
        assert_eq!(chosen.iter().map(|(n, _)| *n).collect::<Vec<_>>(), [a]);
        assert!(select(&heard, &[node_arg("00:01").unwrap()]).is_err());
        let error = select(&heard, &[node_arg("B0").unwrap()]).unwrap_err().to_string();
        assert!(error.contains("several nodes"), "{error}");
    }

    #[test]
    fn ota_cmd_refuses_node_arg_when_it_is_not_an_address() {
        assert!(node_arg("/dev/ttyACM0").is_err());
        assert!(node_arg("3C:DC:75:84:A1:B0").is_ok());
    }
}
