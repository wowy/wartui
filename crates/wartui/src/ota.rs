//! `wartui ota` — update nodes over the node bus, all at once, through the bridge.
//!
//! **The update is `wartui_app::update::nodes`'s**, and so is its reasoning: why it listens
//! first, why the plan is the decision, why the image crosses the host link once, why every node
//! updates at once, and why the scan stops first. This command adds the flags, the link and the
//! bridge, where each board's image comes from, the table, the question, the drawing and the exit.
//!
//! **It listens for `--listen` seconds**, 6 by default: long enough for an assigned node, which
//! beats every 5 s, to be heard once.
//!
//! **The table is shown before anything is sent**, and nothing is sent until the operator says
//! yes, or `--yes` said it already.
//!
//! **The link is picked as every command picks it** (`crate::link_for`): `--link`, else a
//! `--bridge` path's USB, else `[bridge] link`. Over Bluetooth the bridge is the one named or
//! remembered there (`reset::bluetooth_target`), never one a scan hears, whose fleet would take
//! the image. The upload and every session's records go through the one `LinkHandle` as over USB,
//! more slowly.
//!
//! **A ctrl-c ends the run through `main`, which closes a Bluetooth link.** While connecting,
//! listening, finding images, asking and stopping the scan, it ends the run at once
//! ([`unless_stopped`]); once sessions run, it cancels them, so each node is told before the run
//! ends.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args as ClapArgs;
use wartui_app::update::image::NodeImage;
use wartui_app::update::nodes::{
    self, Action, CutShort, Failed, Plan, Progress as Said, Row, SelectError, Target,
};
use wartui_bridge::LinkHandle;
use wartui_bridge::sim::{SimConfig, SimTransport, minor_behind};

use crate::config::Link;
use wartui_core::ota::{Outcome, Phase, Progress};
use wartui_proto::bus::{Board, NODE_VERSION, NodeBoard, Slot, UpdateState};
use wartui_proto::mac::{self, Mac};
use wartui_proto::radio_link::update;

use crate::flash::app::{self, board_name};
use crate::text::node_name;

#[derive(ClapArgs)]
pub struct Args {
    /// The bridge, as a device path, the board's address, or the end of it (`00:08`). If
    /// omitted, detected over USB, and over Bluetooth the one remembered there.
    #[arg(long, value_name = "PATH|MAC")]
    bridge: Option<String>,

    #[command(flatten)]
    link_flag: crate::config::LinkFlag,

    /// Read the remembered bridge from this file instead of the default `wartui.toml`. A file
    /// that does not load is no bridge remembered. Never written; see
    /// `crates/wartui/README.md` § "Config file".
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// A node to update, by the slot name the fleet table shows (`N3`). Repeatable. Every node
    /// heard by default.
    #[arg(long = "node", value_name = "SLOT")]
    nodes: Vec<String>,

    /// Send this app-only image, to the nodes of the board it was built for, rather than
    /// building or downloading one.
    #[arg(long, value_name = "BIN", conflicts_with = "firmware_dir")]
    image: Option<PathBuf>,

    /// Build the image from the node firmware in this directory.
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

/// The slots `names` mean on `board`. A bridge with no carrier has no slot names to take.
fn slots_named(board: NodeBoard, names: &[String]) -> Result<Vec<Slot>, String> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    if !board.is_carrier() {
        return Err("this bridge has no carrier board, so no slot has a name to pick a node by; \
                    leave out --node to update every node heard"
            .to_owned());
    }
    names.iter().map(|name| crate::text::slot_named(board, name)).collect()
}

/// What the run does with one node, as the table says it.
fn action_text(action: &Action) -> String {
    match action {
        Action::Update => "update".to_owned(),
        Action::Confirm => "confirm".to_owned(),
        Action::Same(version) => format!("skip: already {version}"),
        Action::Older => "skip: older image (use --force)".to_owned(),
        Action::NoImage(board) => format!("refuse: no image for {}", board_name(*board)),
    }
}

/// Why no node was chosen, as the operator reads it, on `board` after listening `listened`.
fn select_text(error: SelectError, board: NodeBoard, listened: Duration) -> String {
    match error {
        SelectError::NoneHeard => {
            "no node heartbeated; check they are powered and in range of the bridge".to_owned()
        }
        SelectError::NoneInSlot(slot) => {
            format!("no node heartbeated from {} in {}s", board.label(slot), listened.as_secs())
        }
    }
}

pub async fn run(args: Args) -> Result<()> {
    if !args.yes && !std::io::stdin().is_terminal() {
        bail!("standard input is not a terminal, so nothing can answer; pass --yes to update");
    }
    let (mut link, over, bridge) = open(&args)?;
    let mut terminate = crate::Terminate::new();
    let stop = async {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            () = terminate.recv() => {}
        }
    };
    tokio::pin!(stop);
    let ready = crate::status::wait_for_ready(&mut link, over, super::connect_wait(over));
    let Some(info) = unless_stopped(&mut stop, ready).await?? else {
        bail!("{}", super::no_bridge_notice(bridge.as_deref(), over).await)
    };
    let board = info.node_board;
    let named = slots_named(board, &args.nodes).map_err(anyhow::Error::msg)?;
    println!("bridge  {}, firmware {}", mac::full(&info.mac), info.fw_version);
    println!("listening {} s for heartbeats", args.listen);
    let listen_for = Duration::from_secs(args.listen);
    let heard = unless_stopped(&mut stop, nodes::listen(&mut link, listen_for)).await??;
    let targets = nodes::select(&heard, &named)
        .map_err(|error| anyhow!(select_text(error, board, listen_for)))?;
    let names: HashMap<Mac, String> = targets
        .iter()
        .map(|target| (target.node, node_name(board, target.slot, &target.node)))
        .collect();

    let images = unless_stopped(&mut stop, images(&args, &targets)).await??;
    let plan = Plan::of(targets, images, args.force);
    print_table(plan.rows(), &names);

    if plan.chosen() == 0 {
        println!("nothing to update");
        return Ok(());
    }
    if !args.yes {
        let asked = tokio::task::spawn_blocking(confirm);
        if !unless_stopped(&mut stop, asked).await?.context("asking")?? {
            println!("nothing updated");
            return Ok(());
        }
    }

    let mut view: Option<FleetView> = None;
    let say = |said: Said| match said {
        Said::ScanNotStopped => {
            println!("the bridge did not report its scan stopped; updating anyway")
        }
        Said::Fleet { nodes, image_bytes, .. } => {
            let lines = nodes.iter().map(|node| (*node, names[node].clone())).collect();
            view = Some(FleetView::new(lines, image_bytes));
        }
        Said::Cached { took } => {
            if let Some(view) = &mut view {
                view.cached(took);
            }
        }
        Said::Node { node, progress } => {
            if let Some(view) = &mut view {
                view.show(node, &progress);
            }
        }
        Said::FleetEnded { outcomes } => {
            if let Some(view) = &mut view {
                view.finish(&outcomes);
            }
        }
    };
    let ended = nodes::run(&mut link, &plan, &mut stop, say).await;
    if ended.cut_short == Some(CutShort::BeforeStart) {
        return Err(crate::Interrupted.into());
    }
    ended.verdict().map_err(|failed| anyhow!(failed_text(failed)))
}

/// How a run that did not update every chosen node ended, as the operator reads it.
fn failed_text(failed: Failed) -> String {
    match failed {
        Failed::Upload { failure, done } => {
            let why = cache_failure(failure);
            let updated = match done {
                0 => "nothing was updated".to_owned(),
                1 => "1 node was updated before it".to_owned(),
                n => format!("{n} nodes were updated before it"),
            };
            format!("the bridge did not take the node image ({why}); {updated}")
        }
        Failed::CacheLost => "the bridge lost the node image; nodes already confirmed keep the \
                              new image, and running `wartui ota` again updates the rest from \
                              the start"
            .to_owned(),
        Failed::Cancelled => "cancelled".to_owned(),
        Failed::Nodes { failed, total } => format!("{failed} of {total} nodes failed to update"),
    }
}

/// Why the cache refused or lost an upload, for an operator.
fn cache_failure(failure: update::Failure) -> String {
    match failure {
        update::Failure::Refused(UpdateState::FlashError) => {
            "it has no PSRAM for its node image cache".to_owned()
        }
        update::Failure::Refused(UpdateState::TooLarge) => {
            "the image is too large for its cache".to_owned()
        }
        update::Failure::Refused(UpdateState::HashMismatch) => {
            "the image arrived corrupted (hash mismatch)".to_owned()
        }
        update::Failure::Refused(state) | update::Failure::SwitchFailed(state) => {
            format!("it refused the image: {state:?}")
        }
        update::Failure::Silent => "it stopped answering".to_owned(),
        update::Failure::LostSession => "it kept dropping the upload".to_owned(),
    }
}

/// Open the simulator, or the bridge [`reach`] picks. Returns the handle, the link and the bridge
/// it was named by, if any.
fn open(args: &Args) -> Result<(LinkHandle, Link, Option<String>)> {
    if let Some(node_count) = args.sim {
        let config = SimConfig {
            node_count,
            fleet_version: minor_behind(NODE_VERSION),
            ..SimConfig::default()
        };
        let link = SimTransport::new(config).start().context("starting the simulator")?;
        return Ok((link, Link::Usb, None));
    }
    let remembered = crate::config::remembered_bridge(args.config.as_deref());
    let (over, bridge) = reach(args, &remembered)?;
    let link = super::open(bridge.as_deref(), None, over, super::memory_of(&remembered))?;
    Ok((link, over, bridge))
}

/// The link `crate::link_for` picks, given the `[bridge]` table `wartui.toml` holds, and the
/// bridge to name on it: over USB `--bridge`, and over Bluetooth the bridge named or remembered
/// there ([`crate::reset::bluetooth_target`]).
fn reach(args: &Args, remembered: &crate::config::Bridge) -> Result<(Link, Option<String>)> {
    let named = args.bridge.as_deref();
    let over = super::link_for(args.link_flag.link, named, remembered.link)?;
    let bridge = match over {
        Link::Usb => named.map(str::to_owned),
        Link::Bluetooth => {
            let target = crate::reset::bluetooth_target(named, remembered, "ota")
                .map_err(anyhow::Error::msg)?;
            Some(target.to_string())
        }
    };
    Ok((over, bridge))
}

/// `step`'s output, or [`crate::Interrupted`] when `stop` resolves first.
async fn unless_stopped<T>(
    stop: impl Future<Output = ()>,
    step: impl Future<Output = T>,
) -> Result<T> {
    tokio::select! {
        out = step => Ok(out),
        () = stop => Err(crate::Interrupted.into()),
    }
}

/// One image per board among `targets`: `--image` for its own board, else a build or a
/// release download per board.
async fn images(args: &Args, targets: &[Target]) -> Result<HashMap<Board, NodeImage>> {
    let boards = nodes::boards(targets);
    let image = args.image.clone();
    let firmware_dir = args.firmware_dir.clone();
    // Builds and downloads block, and the link's reader must keep running meanwhile.
    let found = tokio::task::spawn_blocking(move || -> Result<Vec<NodeImage>> {
        if let Some(path) = image {
            let image = app::load(&path)?;
            println!(
                "image   {} for {}, {}",
                image.version(),
                board_name(image.board()),
                path.display()
            );
            return Ok(vec![image]);
        }
        let mut found = Vec::new();
        for board in boards {
            let source = app::source_for(board, firmware_dir.as_deref())?;
            println!("image   {}: {}", board_name(board), source.describe(false));
            let image = app::obtain(board, &source)?;
            println!("image   {} for {}", image.version(), board_name(board));
            found.push(image);
        }
        Ok(found)
    })
    .await
    .context("finding images")??;
    Ok(found.into_iter().map(|image| (image.board(), image)).collect())
}

fn print_table(plan: &[Row], names: &HashMap<Mac, String>) {
    println!();
    for line in table(plan, names) {
        println!("{line}");
    }
    println!();
}

/// The table's lines, header first, each node by the name in `names`.
fn table(plan: &[Row], names: &HashMap<Mac, String>) -> Vec<String> {
    let mut lines =
        vec![format!("{:<7}  {:<14}  {:<8}  {:<8}  action", "node", "board", "version", "image")];
    for Row { target: Target { node, caps, .. }, image, action } in plan {
        let image = image.map_or_else(|| "—".to_owned(), |v| v.to_string());
        let version =
            if caps.trial { format!("{};trial", caps.version) } else { caps.version.to_string() };
        let name = names.get(node).cloned().unwrap_or_else(|| mac::short(node).to_string());
        lines.push(format!(
            "{name:<7}  {:<14}  {version:<8}  {image:<8}  {}",
            board_name(caps.board),
            action_text(action),
        ));
    }
    lines
}

/// Ask on the terminal. `run` has already refused when standard input is not one.
fn confirm() -> Result<bool> {
    eprint!("Update these nodes? [y/N] ");
    std::io::stderr().flush().context("writing the question")?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer).context("reading the answer")?;
    Ok(matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

/// The upload into the bridge's cache, and every node's progress: on a terminal, a line each
/// redrawn in place, and otherwise a line at each change. Then each node's outcome.
struct FleetView {
    /// The upload's line, while the fleet sends an image.
    cache: Option<CacheLine>,
    /// Each node's line, by its address.
    lines: Vec<(Mac, ProgressLine)>,
    tty: bool,
    /// Rows the last redraw wrote, which the next moves the cursor back over.
    rows: usize,
    drawn: Option<Instant>,
}

/// The upload of an image of `len` bytes into the bridge's cache, and how long it took once the
/// bridge has checked it.
struct CacheLine {
    len: usize,
    took: Option<Duration>,
}

impl CacheLine {
    fn text(&self) -> String {
        let kb = self.len.div_ceil(1024);
        match self.took {
            Some(took) => format!("cache   {kb} KB to the bridge in {:.1} s", took.as_secs_f64()),
            None => format!("cache   sending {kb} KB to the bridge"),
        }
    }
}

impl FleetView {
    /// A view of `nodes`, each by its name, and of an upload of `image` bytes when the fleet
    /// sends an image.
    fn new(nodes: Vec<(Mac, String)>, image: Option<usize>) -> Self {
        Self {
            cache: image.map(|len| CacheLine { len, took: None }),
            lines: nodes.into_iter().map(|(node, name)| (node, ProgressLine::new(name))).collect(),
            tty: std::io::stdout().is_terminal(),
            rows: 0,
            drawn: None,
        }
    }

    /// The bridge checked the image `took` after the fleet started.
    fn cached(&mut self, took: Duration) {
        let Some(cache) = &mut self.cache else { return };
        cache.took = Some(took);
        if self.tty {
            self.redraw(Instant::now());
        } else {
            println!("{}", cache.text());
        }
    }

    /// Where `node`'s session stands.
    fn show(&mut self, node: Mac, progress: &Progress) {
        let now = Instant::now();
        // A node's rate counts from when it may stream: once the image is cached.
        let cached = self.cache.as_ref().is_none_or(|cache| cache.took.is_some());
        let Some((_, line)) = self.lines.iter_mut().find(|(at, _)| *at == node) else { return };
        let changed = line.update(progress, now, cached);
        if self.tty {
            let due =
                self.drawn.is_none_or(|at| now.duration_since(at) >= Duration::from_millis(250));
            if changed || due {
                self.redraw(now);
            }
        } else if changed && !matches!(progress.phase, Phase::Done | Phase::Failed) {
            // `finish` says how it ended.
            println!("{}", line.render(progress, now));
        }
    }

    /// Show each node's outcome, in place of its progress on a terminal.
    fn finish(&mut self, outcomes: &[(Mac, Outcome)]) {
        self.take_outcomes(outcomes);
        if self.tty {
            self.redraw(Instant::now());
        } else {
            for (_, line) in &self.lines {
                println!("{}", line.text(Instant::now()));
            }
        }
    }

    /// Give each node with an outcome its outcome, on its own line.
    fn take_outcomes(&mut self, outcomes: &[(Mac, Outcome)]) {
        for (node, outcome) in outcomes {
            if let Some((_, line)) = self.lines.iter_mut().find(|(at, _)| at == node) {
                line.outcome = Some(outcome.clone());
            }
        }
    }

    fn redraw(&mut self, now: Instant) {
        let mut text = String::new();
        if self.rows > 0 {
            let _ = write!(text, "\x1b[{}A", self.rows);
        }
        if let Some(cache) = &self.cache {
            let _ = writeln!(text, "\r\x1b[K{}", cache.text());
        }
        for (_, line) in &self.lines {
            let _ = writeln!(text, "\r\x1b[K{}", line.text(now));
        }
        print!("{text}");
        let _ = std::io::stdout().flush();
        self.rows = self.lines.len() + usize::from(self.cache.is_some());
        self.drawn = Some(now);
    }
}

/// One node's progress, and how it ended.
struct ProgressLine {
    node: String,
    progress: Option<Progress>,
    outcome: Option<Outcome>,
    /// When streaming started, and how much the node held then, for the rate.
    streaming: Option<(Instant, u32)>,
    /// When the node first held the whole image, which stops the rate's clock.
    streamed: Option<Instant>,
}

impl ProgressLine {
    fn new(node: String) -> Self {
        Self { node, progress: None, outcome: None, streaming: None, streamed: None }
    }

    /// Take the latest progress, returning whether its phase changed. The rate's clock starts
    /// once the node is streaming and the image is `cached`.
    fn update(&mut self, progress: &Progress, now: Instant, cached: bool) -> bool {
        if cached && progress.phase == Phase::Streaming && self.streaming.is_none() {
            self.streaming = Some((now, progress.acked));
        }
        if progress.acked == progress.total && self.streamed.is_none() {
            self.streamed = Some(now);
        }
        let changed = self.progress.is_none_or(|last| last.phase != progress.phase);
        self.progress = Some(*progress);
        changed
    }

    fn text(&self, now: Instant) -> String {
        match (&self.outcome, &self.progress) {
            (Some(outcome), _) => {
                let result = match outcome {
                    Outcome::Done(version) => format!("done {version} (confirmed)"),
                    Outcome::Failed(failure) => format!("failed: {failure}"),
                };
                format!("{:<7}  {result}", self.node)
            }
            (None, Some(progress)) => self.render(progress, now),
            (None, None) => format!("{:<7}  waiting", self.node),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    use wartui_app::update::nodes::Heard;
    use wartui_bridge::spec::BridgeSpec;
    use wartui_core::ota::Failure;
    use wartui_proto::bus::{Capabilities, Version};

    const NEW: Version = Version::new(0, 1, 1);

    /// A bridge's address, locally administered.
    const BRIDGE: Mac = [0x02, 0x00, 0x5E, 0x10, 0x44, 0xC0];

    /// `ota` as typed with `bridge` and `link`.
    fn args(bridge: Option<&str>, link: Option<Link>) -> Args {
        Args {
            bridge: bridge.map(str::to_owned),
            link_flag: crate::config::LinkFlag { link },
            config: None,
            nodes: Vec::new(),
            image: None,
            firmware_dir: None,
            listen: 6,
            force: false,
            yes: true,
            sim: None,
        }
    }

    #[test]
    fn ota_cmd_refuses_unnamed_bridge_when_over_bluetooth_with_none_remembered_there() {
        // Remembered over USB only: only a scan would find it, and a scan may hear another
        // fleet's bridge, whose nodes would take this image.
        let usb_only = crate::config::Bridge {
            mac: Some(BRIDGE),
            link: Some(Link::Bluetooth),
            ..crate::config::Bridge::default()
        };
        let refused = reach(&args(None, None), &usb_only).unwrap_err().to_string();
        assert!(refused.contains("`wartui ota`"), "{refused}");
        assert!(refused.contains("--bridge XX:YY"), "{refused}");
        let (over, bridge) = reach(&args(Some("44:C0"), None), &usb_only).unwrap();
        assert_eq!((over, bridge.as_deref()), (Link::Bluetooth, Some("44:C0")), "named");
        // Over USB the transport's own choice stands.
        let (over, bridge) = reach(&args(None, Some(Link::Usb)), &usb_only).unwrap();
        assert_eq!((over, bridge), (Link::Usb, None));
    }

    #[test]
    fn ota_cmd_names_remembered_bridge_when_over_bluetooth_with_it_remembered_there() {
        let remembered = crate::config::Bridge {
            mac: Some(BRIDGE),
            bluetooth_address: Some("02:00:5E:10:44:C2".to_owned()),
            link: Some(Link::Bluetooth),
            ..crate::config::Bridge::default()
        };
        let (over, bridge) = reach(&args(None, None), &remembered).unwrap();
        let mac = BridgeSpec::Mac(BRIDGE).to_string();
        assert_eq!((over, bridge), (Link::Bluetooth, Some(mac)));
    }

    fn caps(version: Version, trial: bool) -> Capabilities {
        Capabilities { version, trial, ..Capabilities::here(Board::Esp32c5) }
    }

    #[test]
    fn ota_cmd_words_each_action_when_printing_table() {
        assert_eq!(action_text(&Action::Update), "update");
        assert_eq!(action_text(&Action::Confirm), "confirm");
        assert_eq!(action_text(&Action::Same(NEW)), "skip: already 0.1.1");
        assert_eq!(action_text(&Action::Older), "skip: older image (use --force)");
        assert_eq!(action_text(&Action::NoImage(Board::Esp32c6)), "refuse: no image for esp32c6");
    }

    #[test]
    fn ota_cmd_selects_nodes_by_slot_when_named() {
        // A carrier-14 with node 0 in A1, `N1`, and node 1 in A2.
        let nodes = [[0x02, 0, 0, 0, 0, 1], [0x02, 0, 0, 0, 0, 2]];
        let heard = Heard {
            nodes: nodes.iter().map(|node| (*node, caps(NEW, false))).collect(),
            slots: HashMap::from([
                (nodes[0], Slot::new(0, 0).unwrap()),
                (nodes[1], Slot::new(0, 1).unwrap()),
            ]),
        };
        let board = NodeBoard::Carrier14;
        let listened = Duration::from_secs(6);
        let pick = |names: &[&str]| {
            let names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
            let named = slots_named(board, &names).map_err(anyhow::Error::msg)?;
            nodes::select(&heard, &named)
                .map_err(|error| anyhow!(select_text(error, board, listened)))
        };

        assert_eq!(pick(&[]).unwrap().len(), 2, "none named picks every node heard");
        let chosen = pick(&["N1"]).unwrap();
        assert_eq!(chosen.iter().map(|target| target.node).collect::<Vec<_>>(), [nodes[0]]);
        let error = pick(&["N3"]).unwrap_err().to_string();
        assert_eq!(error, "no node heartbeated from N3 in 6s");
        let error = pick(&["N15"]).unwrap_err().to_string();
        assert!(error.contains("N1 to N14"), "{error}");
        let error = select_text(SelectError::NoneHeard, board, listened);
        assert_eq!(error, "no node heartbeated; check they are powered and in range of the bridge");
    }

    #[test]
    fn ota_cmd_refuses_slot_name_when_bridge_has_no_carrier() {
        assert_eq!(slots_named(NodeBoard::NoCarrier, &[]), Ok(Vec::new()));
        let refused = slots_named(NodeBoard::NoCarrier, &["N1".to_owned()]).expect_err("refused");
        assert!(refused.contains("no carrier board"), "{refused}");
    }

    #[test]
    fn ota_cmd_names_node_by_slot_when_printing_table() {
        let (named, slotless) = ([0x02, 0, 0, 0, 0, 1], [0x02, 0, 0, 0, 0x57, 0x84]);
        let board = NodeBoard::Carrier14;
        let names: HashMap<Mac, String> = [
            (named, node_name(board, Slot::new(0, 0), &named)),
            (slotless, node_name(board, None, &slotless)),
        ]
        .into();
        let row = |node, caps, action| Row {
            target: Target { node, caps, slot: None },
            image: Some(NEW),
            action,
        };
        let plan = [
            row(named, caps(Version::new(0, 0, 9), false), Action::Update),
            row(slotless, caps(NEW, true), Action::Same(NEW)),
        ];
        let lines = table(&plan, &names);
        assert_eq!(lines[0], "node     board           version   image     action");
        assert_eq!(lines[1], "N1       esp32c5         0.0.9     0.1.1     update");
        assert!(lines[2].starts_with("57:84    esp32c5         0.1.1;trial"), "{lines:?}");
        assert!(lines[2].ends_with("skip: already 0.1.1"), "{lines:?}");
    }

    #[test]
    fn ota_cmd_counts_earlier_updates_when_a_later_upload_fails() {
        let failure = update::Failure::Silent;
        let error = failed_text(Failed::Upload { failure, done: 2 });
        let expected = "the bridge did not take the node image (it stopped answering); 2 nodes \
                        were updated before it";
        assert_eq!(error, expected);
        let error = failed_text(Failed::Upload { failure, done: 1 });
        assert!(error.ends_with("; 1 node was updated before it"), "{error}");
        let error = failed_text(Failed::Upload { failure, done: 0 });
        assert!(error.ends_with("; nothing was updated"), "{error}");
    }

    #[test]
    fn ota_cmd_words_each_failure_when_a_run_fails() {
        let error = failed_text(Failed::CacheLost);
        assert!(error.starts_with("the bridge lost the node image;"), "{error}");
        assert_eq!(failed_text(Failed::Cancelled), "cancelled");
        let error = failed_text(Failed::Nodes { failed: 1, total: 4 });
        assert_eq!(error, "1 of 4 nodes failed to update");
    }

    #[test]
    fn ota_cmd_words_cache_refusal_when_bridge_names_its_state() {
        let refused = update::Failure::Refused(UpdateState::TooLarge);
        assert_eq!(cache_failure(refused), "the image is too large for its cache");
        assert_eq!(cache_failure(update::Failure::LostSession), "it kept dropping the upload");
    }

    #[test]
    fn ota_cmd_shows_outcome_on_its_nodes_line_when_outcomes_arrive_out_of_order() {
        let (first, second) = ([0x02, 0, 0, 0, 0, 1], [0x02, 0, 0, 0, 0, 2]);
        let lines = vec![(first, "N1".to_owned()), (second, "N2".to_owned())];
        let mut view = FleetView::new(lines, None);
        let failed = Outcome::Failed(Failure::RolledBack(Version::new(0, 0, 9)));
        view.take_outcomes(&[(second, failed), (first, Outcome::Done(NEW))]);
        let now = Instant::now();
        let texts: Vec<String> = view.lines.iter().map(|(_, line)| line.text(now)).collect();
        assert_eq!(
            texts,
            ["N1       done 0.1.1 (confirmed)", "N2       failed: rolled back to 0.0.9"]
        );
    }

    #[test]
    fn ota_cmd_keeps_progress_on_a_nodes_line_when_it_has_no_outcome() {
        let (first, second) = ([0x02, 0, 0, 0, 0, 1], [0x02, 0, 0, 0, 0, 2]);
        let lines = vec![(first, "N1".to_owned()), (second, "N2".to_owned())];
        let mut view = FleetView::new(lines, None);
        view.take_outcomes(&[(second, Outcome::Done(NEW))]);
        let now = Instant::now();
        assert_eq!(view.lines[0].1.text(now), "N1       waiting");
        assert_eq!(view.lines[1].1.text(now), "N2       done 0.1.1 (confirmed)");
    }
}
