//! `wartui ota` — update nodes over the node bus, all at once, through the bridge.
//!
//! **It listens before it acts.** A node is reached through the slot its heartbeat came from,
//! and only a heartbeat says what board and version it is, so the run gathers them for
//! `--listen` seconds first: long enough for an assigned node, which beats every 5 s, to be
//! heard once. Only the heartbeat's frozen prefix is read, so a node a minor version behind,
//! which `run` cannot drive, is exactly the node this reaches.
//!
//! **The table is the decision, and it is shown before anything is sent.** Each node's action
//! follows from its version and whether there is an image for its board. A node on trial is
//! updated like any other: it confirms its running image on the first update frame it decodes,
//! before taking the update into its other slot.
//!
//! **The image crosses the host link once.** Every node of a board takes the same image, so the
//! run uploads it into the bridge's node image cache, and each session names its chunks by
//! offset. Ten nodes cost one upload rather than ten copies of the image on the wire. The upload
//! runs beside the sessions' `begin`s ([`drive_fleet`]), so a node is ready to take chunks by the
//! time the bridge has checked the image, and none is sent one before. The bridge caches one
//! image, so a run with images for two boards updates one board's nodes, then the other's
//! ([`fleets`]).
//!
//! **Every chosen node updates at once**, one session each over the one link
//! ([`drive_fleet`]). The host link stops being the limit once the image is cached, so the two
//! buses side by side are what an update waits on. There is no canary: an image that boots and
//! misbehaves is caught in development, not here. Each session keeps every safety net of its own,
//! the trial boot and rollback included, so a node that fails leaves the others running and its
//! own old image in place.
//!
//! **Each session starts from the slot the listen heard its node in**, so its `begin` goes at
//! once rather than on the node's next heartbeat. Events that queued while the run was busy are
//! thrown away before the sessions start: a heartbeat that waited out an image build or the
//! prompt is history. A node that moved since is found by its next heartbeat.
//!
//! **The scan stops before the first update record** ([`stop_scan`]), so the P4 sends the nodes
//! nothing of its own while they update, and the fleet stays idle afterwards until a run starts
//! it.
//!
//! [`wartui_core::ota`] has the session's own reasoning.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::future::Future;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use wartui_bridge::sim::{SimConfig, SimTransport, minor_behind};
use wartui_bridge::{LinkEvent, LinkHandle};
use wartui_core::ota::{Failure, Ids, OtaImage, OtaSession, Outcome, Phase, Progress};
use wartui_core::runtime::{FleetError, FleetReport, drive_fleet, now, stop_scan};
use wartui_proto::bus::{
    Board, Capabilities, Heartbeat, NODE_VERSION, NodeBoard, Slot, UpType, UpdateState, Version,
};
use wartui_proto::link::BridgeToHost;
use wartui_proto::mac::{self, Mac};
use wartui_proto::radio_link::update;

use crate::flash::app::{self, AppImage, board_name};
use crate::text::node_name;

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
        value_parser = crate::sim_nodes()
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

/// How long `ota` waits for the bridge to report its scan stopped before updating anyway.
const SCAN_STOP_WAIT: Duration = Duration::from_secs(2);

pub async fn run(args: Args) -> Result<()> {
    if !args.yes && !std::io::stdin().is_terminal() {
        bail!("standard input is not a terminal, so nothing can answer; pass --yes to update");
    }
    let mut link = open(&args)?;
    let info = match tokio::time::timeout(super::CONNECT_NOTICE_AFTER, connected(&mut link)).await {
        Ok(info) => info?,
        Err(_) => bail!("{}", super::no_bridge_notice(args.bridge.as_deref())),
    };
    let board = info.node_board;
    let named = slots_named(board, &args.nodes).map_err(anyhow::Error::msg)?;
    println!("bridge  {}, firmware {}", mac::full(&info.mac), info.fw_version);
    println!("listening {} s for heartbeats", args.listen);
    let listen_for = Duration::from_secs(args.listen);
    let (heard, slots) = listen(&mut link, listen_for).await?;
    let targets = select(&heard, &slots, &named, board, listen_for)?;
    let names: HashMap<Mac, String> = targets
        .iter()
        .map(|(node, _)| (*node, node_name(board, slots.get(node).copied(), node)))
        .collect();

    let images = images(&args, &targets).await?;
    let plan: Vec<(Mac, Capabilities, Option<Version>, Action)> = targets
        .iter()
        .map(|(node, caps)| {
            let image = images.get(&caps.board).map(|image| image.version);
            (*node, *caps, image, decide(caps, image, args.force))
        })
        .collect();
    print_table(&plan, &names);

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

    // Stop the scan first, so the P4 sends the nodes nothing of its own while they update.
    if !stop_scan(&mut link, SCAN_STOP_WAIT).await {
        println!("the bridge did not report its scan stopped; updating anyway");
    }

    // From the clock, so a host restarted mid-update is unlikely to reuse the byte; the node
    // copes if it does.
    let base = u8::try_from(chrono::Utc::now().timestamp().rem_euclid(256)).unwrap_or(1);
    let mut terminate = crate::Terminate::new();
    let stop = async {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            () = terminate.recv() => {}
        }
    };
    tokio::pin!(stop);
    let mut outcomes = Vec::new();
    for (board, members) in fleets(&chosen) {
        let image = board.map(|board| &images[&board].ota);
        let mut sessions: Vec<OtaSession> = members
            .iter()
            .map(|&i| {
                let (node, caps, confirm_only) = chosen[i];
                let ids = Ids::of(i, chosen.len());
                match image {
                    Some(image) if !confirm_only => {
                        let session = base.wrapping_add(u8::try_from(i).unwrap_or(0));
                        let session =
                            OtaSession::new(node, caps.version, image, session, ids, now());
                        match slots.get(&node) {
                            Some(slot) => session.with_slot(*slot),
                            None => session,
                        }
                    }
                    _ => OtaSession::confirm_trial(node, caps.version, ids, now()),
                }
            })
            .collect();
        let nodes: Vec<String> = members.iter().map(|&i| names[&chosen[i].0].clone()).collect();
        let mut view = FleetView::new(&nodes, image.map(|image| image.bytes.len()));
        let ended =
            update_fleet(&mut link, image, &mut sessions, &mut stop, |r| view.show(r)).await;
        // A fleet the bridge stopped still shows which nodes finished before it did.
        let (got, stopped) = match ended {
            Ok(got) => (got, None),
            Err(error) => {
                let got = sessions.iter().filter_map(|session| session.outcome().cloned());
                (got.collect(), Some(error))
            }
        };
        view.finish(&got);
        outcomes.extend(got);
        if stopped.is_some() {
            return verdict(&outcomes, stopped);
        }
        if outcomes.contains(&Outcome::Failed(Failure::Cancelled)) {
            break;
        }
    }
    verdict(&outcomes, None)
}

/// How a run ends: a fleet the bridge stopped, cancelled, a count of the nodes that failed, or
/// nothing to report. `outcomes` are every node's that has one, `stopped`'s fleet included.
fn verdict(outcomes: &[Outcome], stopped: Option<FleetError>) -> Result<()> {
    match stopped {
        Some(FleetError::Upload(failure)) => {
            let why = cache_failure(failure);
            let done =
                outcomes.iter().filter(|outcome| matches!(outcome, Outcome::Done(_))).count();
            let updated = match done {
                0 => "nothing was updated".to_owned(),
                1 => "1 node was updated before it".to_owned(),
                n => format!("{n} nodes were updated before it"),
            };
            bail!("the bridge did not take the node image ({why}); {updated}");
        }
        Some(FleetError::CacheLost) => bail!(
            "the bridge lost the node image; nodes already confirmed keep the new image, and \
             running `wartui ota` again updates the rest from the start"
        ),
        None => {}
    }
    if outcomes.contains(&Outcome::Failed(Failure::Cancelled)) {
        bail!("cancelled");
    }
    let failed = outcomes.iter().filter(|outcome| !matches!(outcome, Outcome::Done(_))).count();
    if failed > 0 {
        bail!("{failed} of {} nodes failed to update", outcomes.len());
    }
    Ok(())
}

/// The chosen nodes as the fleets that run one after another, as indices into `chosen`: one per
/// board with an image to send, since the bridge caches one image, and the confirms with the
/// first. A fleet of confirms alone sends no image.
fn fleets(chosen: &[(Mac, Capabilities, bool)]) -> Vec<(Option<Board>, Vec<usize>)> {
    let mut fleets: Vec<(Option<Board>, Vec<usize>)> = Vec::new();
    for (i, (_, caps, confirm_only)) in chosen.iter().enumerate() {
        if *confirm_only {
            continue;
        }
        match fleets.iter_mut().find(|(board, _)| *board == Some(caps.board)) {
            Some((_, members)) => members.push(i),
            None => fleets.push((Some(caps.board), vec![i])),
        }
    }
    let confirms = chosen.iter().enumerate().filter(|(_, (.., confirm))| *confirm).map(|(i, _)| i);
    match fleets.first_mut() {
        Some((_, members)) => members.extend(confirms),
        None => fleets.push((None, confirms.collect())),
    }
    fleets
}

/// Update one fleet: drive every session at once, uploading `image` into the bridge's cache
/// meanwhile when the fleet sends one, until each has an outcome or `stop` resolves.
async fn update_fleet(
    link: &mut LinkHandle,
    image: Option<&OtaImage>,
    sessions: &mut [OtaSession],
    stop: impl Future<Output = ()>,
    report: impl FnMut(FleetReport<'_>),
) -> Result<Vec<Outcome>, FleetError> {
    drain(link).await;
    drive_fleet(link, sessions, image, stop, report).await
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
    super::open(bridge, None, super::memory(args.config.as_deref()))
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

/// Every node heartbeating within `period`, as its latest heartbeat's prefix says, and the slot
/// that heartbeat came from. A slot names the node heard from it last: a node another node's
/// heartbeat displaced keeps its place among those heard, with no slot.
async fn listen(
    link: &mut LinkHandle,
    period: Duration,
) -> Result<(BTreeMap<Mac, Capabilities>, HashMap<Mac, Slot>)> {
    let mut heard = BTreeMap::new();
    let mut slots = HashMap::new();
    let deadline = tokio::time::Instant::now() + period;
    loop {
        let event = match tokio::time::timeout_at(deadline, link.recv()).await {
            Err(_) => return Ok((heard, slots)),
            Ok(None) => bail!("the link closed"),
            Ok(Some(event)) => event,
        };
        if let LinkEvent::Message(BridgeToHost::FromNode { slot, kind, body, .. }) = event
            && kind == UpType::Heartbeat.as_u8()
            && let Ok((caps, mac)) = Heartbeat::identity(&body)
        {
            heard.insert(mac, caps);
            slots.retain(|_, held| *held != slot);
            slots.insert(mac, slot);
        }
    }
}

/// Throw away whatever queued while the run was doing something else.
async fn drain(link: &mut LinkHandle) {
    while let Ok(Some(_)) = tokio::time::timeout(Duration::ZERO, link.recv()).await {}
}

/// The nodes heard in the `named` slots of `board`, or every one heard when none is named.
fn select(
    heard: &BTreeMap<Mac, Capabilities>,
    slots: &HashMap<Mac, Slot>,
    named: &[Slot],
    board: NodeBoard,
    listened: Duration,
) -> Result<Vec<(Mac, Capabilities)>> {
    if heard.is_empty() {
        bail!("no node heartbeated; check they are powered and in range of the bridge");
    }
    if named.is_empty() {
        return Ok(heard.iter().map(|(node, caps)| (*node, *caps)).collect());
    }
    let mut chosen = BTreeMap::new();
    for slot in named {
        let Some((node, caps)) = slots
            .iter()
            .find(|(_, held)| *held == slot)
            .and_then(|(node, _)| heard.get(node).map(|caps| (*node, *caps)))
        else {
            bail!("no node heartbeated from {} in {}s", board.label(*slot), listened.as_secs());
        };
        chosen.insert(node, caps);
    }
    Ok(chosen.into_iter().collect())
}

/// An image ready to send.
struct Ready {
    version: Version,
    ota: OtaImage,
}

/// The boards among `targets` a node image exists for, each once.
///
/// One with none is left out, so the table refuses that node and the run goes on. Asking for
/// its image would stop the update for every other node too.
fn boards_with_images(targets: &[(Mac, Capabilities)]) -> Vec<Board> {
    let mut boards: Vec<Board> = targets
        .iter()
        .map(|(_, caps)| caps.board)
        .filter(|board| app::has_node_image(*board))
        .collect();
    boards.sort_by_key(|board| board.as_u8());
    boards.dedup();
    boards
}

/// One image per board among `targets`: `--image` for its own board, else a build or a
/// release download per board.
async fn images(args: &Args, targets: &[(Mac, Capabilities)]) -> Result<HashMap<Board, Ready>> {
    let boards = boards_with_images(targets);
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

fn print_table(
    plan: &[(Mac, Capabilities, Option<Version>, Action)],
    names: &HashMap<Mac, String>,
) {
    println!();
    for line in table(plan, names) {
        println!("{line}");
    }
    println!();
}

/// The table's lines, header first, each node by the name in `names`.
fn table(
    plan: &[(Mac, Capabilities, Option<Version>, Action)],
    names: &HashMap<Mac, String>,
) -> Vec<String> {
    let mut lines =
        vec![format!("{:<7}  {:<14}  {:<8}  {:<8}  action", "node", "board", "version", "image")];
    for (node, caps, image, action) in plan {
        let image = image.map_or_else(|| "—".to_owned(), |v| v.to_string());
        let version =
            if caps.trial { format!("{};trial", caps.version) } else { caps.version.to_string() };
        let name = names.get(node).cloned().unwrap_or_else(|| mac::short(node).to_string());
        lines.push(format!(
            "{name:<7}  {:<14}  {version:<8}  {image:<8}  {action}",
            board_name(caps.board),
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
    lines: Vec<ProgressLine>,
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
    /// A view of the nodes `nodes` names, and of an upload of `image` bytes when the fleet sends
    /// an image.
    fn new(nodes: &[String], image: Option<usize>) -> Self {
        Self {
            cache: image.map(|len| CacheLine { len, took: None }),
            lines: nodes.iter().map(|node| ProgressLine::new(node.clone())).collect(),
            tty: std::io::stdout().is_terminal(),
            rows: 0,
            drawn: None,
        }
    }

    fn show(&mut self, report: FleetReport<'_>) {
        let now = Instant::now();
        let (i, progress) = match report {
            FleetReport::Cached(took) => {
                let Some(cache) = &mut self.cache else { return };
                cache.took = Some(took);
                if self.tty {
                    self.redraw(now);
                } else {
                    println!("{}", cache.text());
                }
                return;
            }
            FleetReport::Session(i, progress) => (i, progress),
        };
        // A node's rate counts from when it may stream: once the image is cached.
        let cached = self.cache.as_ref().is_none_or(|cache| cache.took.is_some());
        let Some(line) = self.lines.get_mut(i) else { return };
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
    fn finish(&mut self, outcomes: &[Outcome]) {
        for (line, outcome) in self.lines.iter_mut().zip(outcomes) {
            line.outcome = Some(outcome.clone());
        }
        if self.tty {
            self.redraw(Instant::now());
        } else {
            for line in &self.lines {
                println!("{}", line.text(Instant::now()));
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
        for line in &self.lines {
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
    use std::sync::atomic::{AtomicU32, Ordering};

    use sha2::{Digest, Sha256};
    use wartui_proto::bus::UpdateStatus;

    const NEW: Version = Version::new(0, 1, 1);

    fn caps(version: Version, trial: bool) -> Capabilities {
        Capabilities { version, trial, ..Capabilities::here(Board::Esp32c5) }
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
        let node = Capabilities::here(Board::Esp32c6);
        let action = decide(&node, None, true);
        assert_eq!(action.to_string(), "refuse: no image for esp32c6");
    }

    #[test]
    fn ota_cmd_skips_board_when_it_has_no_node_image() {
        // One leftover C6 must not stop the update for every C5 beside it.
        let targets = [
            ([0x02, 0, 0, 0, 0, 1], Capabilities::here(Board::Esp32c6)),
            ([0x02, 0, 0, 0, 0, 2], Capabilities::here(Board::Esp32c5)),
            ([0x02, 0, 0, 0, 0, 3], Capabilities::here(Board::Unknown(3))),
            ([0x02, 0, 0, 0, 0, 4], Capabilities::here(Board::Esp32c5)),
        ];
        assert_eq!(boards_with_images(&targets), [Board::Esp32c5]);
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

    #[tokio::test(start_paused = true)]
    async fn ota_cmd_selects_nodes_by_slot_when_named() {
        // The simulator is a carrier-14 and puts node 0 in A1, `N1`, and node 1 in A2.
        let (_link, nodes, slots) = sim_fleet(2, None).await;
        let heard: BTreeMap<Mac, Capabilities> =
            nodes.iter().map(|node| (*node, caps(NEW, false))).collect();
        let board = NodeBoard::Carrier14;
        let listened = Duration::from_secs(6);
        let pick = |names: &[&str]| {
            let names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
            let named = slots_named(board, &names).map_err(anyhow::Error::msg)?;
            select(&heard, &slots, &named, board, listened)
        };

        assert_eq!(pick(&[]).unwrap().len(), 2, "none named picks every node heard");
        let chosen = pick(&["N1"]).unwrap();
        assert_eq!(chosen.iter().map(|(n, _)| *n).collect::<Vec<_>>(), [nodes[0]]);
        let error = pick(&["N3"]).unwrap_err().to_string();
        assert_eq!(error, "no node heartbeated from N3 in 6s");
        let error = pick(&["N15"]).unwrap_err().to_string();
        assert!(error.contains("N1 to N14"), "{error}");
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
        let plan = [
            (named, caps(Version::new(0, 0, 9), false), Some(NEW), Action::Update),
            (slotless, caps(NEW, false), Some(NEW), Action::Same(NEW)),
        ];
        let lines = table(&plan, &names);
        assert!(lines[1].starts_with("N1       esp32c5"), "{lines:?}");
        assert!(lines[2].starts_with("57:84    esp32c5"), "{lines:?}");
    }

    /// How much faster than real time the fake fleet runs.
    const SPEED: f64 = 20.0;

    /// A simulated fleet of `count` nodes a minor behind, connected, every node heard, and the
    /// slot each was heard in.
    async fn sim_fleet(
        count: u8,
        rejecting: Option<u8>,
    ) -> (LinkHandle, Vec<Mac>, HashMap<Mac, Slot>) {
        let config = SimConfig {
            node_count: count,
            speed: SPEED,
            fleet_version: minor_behind(NODE_VERSION),
            rejecting_node: rejecting,
            ..SimConfig::default()
        };
        let mut link = SimTransport::new(config).start().expect("starting the simulator");
        connected(&mut link).await.expect("the simulator connects");
        let nodes: Vec<Mac> = (0..count).map(SimTransport::node_mac).collect();
        let (heard, slots) = listen(&mut link, Duration::from_secs(1)).await.expect("listening");
        assert!(nodes.iter().all(|node| heard.contains_key(node)), "every node heard");
        (link, nodes, slots)
    }

    /// An image of `sectors` sectors and a bit, for [`NODE_VERSION`].
    fn sim_image(sectors: usize) -> OtaImage {
        let bytes: Vec<u8> = (0..sectors * 4096 + 700).map(|i| (i % 251) as u8).collect();
        let sha256 = Sha256::digest(&bytes).into();
        OtaImage { bytes: Arc::from(bytes), sha256, version: NODE_VERSION, board: Board::Esp32c5 }
    }

    /// One session per node running `from`, as `run` starts them: each from the slot its node
    /// was heard in.
    fn sessions_from(
        nodes: &[Mac],
        slots: &HashMap<Mac, Slot>,
        image: &OtaImage,
        from: Version,
    ) -> Vec<OtaSession> {
        nodes
            .iter()
            .enumerate()
            .map(|(i, node)| {
                let session = 0x40 + u8::try_from(i).expect("a small fleet");
                OtaSession::new(*node, from, image, session, Ids::of(i, nodes.len()), now())
                    .with_slot(slots[node])
            })
            .collect()
    }

    /// [`sessions_from`] nodes a minor behind.
    fn sessions(nodes: &[Mac], slots: &HashMap<Mac, Slot>, image: &OtaImage) -> Vec<OtaSession> {
        sessions_from(nodes, slots, image, minor_behind(NODE_VERSION))
    }

    #[tokio::test(start_paused = true)]
    async fn ota_cmd_updates_every_node_when_nothing_is_lost() {
        let (mut link, nodes, slots) = sim_fleet(4, None).await;
        let image = sim_image(6);
        let mut sessions = sessions(&nodes, &slots, &image);
        let outcomes =
            update_fleet(&mut link, Some(&image), &mut sessions, std::future::pending(), |_| {})
                .await
                .expect("the upload");
        assert_eq!(outcomes, vec![Outcome::Done(NODE_VERSION); 4]);
        assert!(verdict(&outcomes, None).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn ota_cmd_finishes_the_rest_when_one_node_rolls_back() {
        let (mut link, nodes, slots) = sim_fleet(4, Some(2)).await;
        let image = sim_image(6);
        let mut sessions = sessions(&nodes, &slots, &image);
        let outcomes =
            update_fleet(&mut link, Some(&image), &mut sessions, std::future::pending(), |_| {})
                .await
                .expect("the upload");
        let old = minor_behind(NODE_VERSION);
        assert_eq!(outcomes[2], Outcome::Failed(Failure::RolledBack(old)));
        for i in [0, 1, 3] {
            assert_eq!(outcomes[i], Outcome::Done(NODE_VERSION), "node {i}");
        }
        let error = verdict(&outcomes, None).unwrap_err().to_string();
        assert_eq!(error, "1 of 4 nodes failed to update");
    }

    #[tokio::test(start_paused = true)]
    async fn ota_cmd_sends_one_abort_per_session_when_cancelled() {
        let (mut link, nodes, slots) = sim_fleet(4, None).await;
        // Long enough that every node is still streaming when the cancel lands.
        let image = sim_image(128);
        let mut sessions = sessions(&nodes, &slots, &image);
        let streaming = tokio::sync::Notify::new();
        let mut seen = vec![false; nodes.len()];
        let progress = |report: FleetReport<'_>| {
            if let FleetReport::Session(i, p) = report {
                seen[i] |= p.phase == Phase::Streaming && p.acked > 0;
            }
            if seen.iter().all(|s| *s) {
                streaming.notify_one();
            }
        };
        let outcomes =
            update_fleet(&mut link, Some(&image), &mut sessions, streaming.notified(), progress)
                .await
                .expect("the upload");
        assert_eq!(outcomes, vec![Outcome::Failed(Failure::Cancelled); 4]);
        // A sim node answers an abort with `Idle`, and nothing else in a stream draws one.
        let mut idle: HashMap<u8, usize> = HashMap::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while let Ok(Some(event)) = tokio::time::timeout_at(deadline, link.recv()).await {
            if let LinkEvent::Message(BridgeToHost::FromNode { kind, body, .. }) = event
                && kind == UpType::UpdateStatus.as_u8()
                && let Ok(status) = UpdateStatus::decode(&body)
                && status.state == UpdateState::Idle
            {
                *idle.entry(status.session).or_default() += 1;
            }
        }
        let expected: HashMap<u8, usize> = (0x40..0x44).map(|session| (session, 1)).collect();
        assert_eq!(idle, expected, "one abort per session");
    }

    #[tokio::test(start_paused = true)]
    async fn ota_cmd_updates_node_when_cache_already_holds_the_image() {
        let (mut link, nodes, slots) = sim_fleet(1, None).await;
        let image = sim_image(3);
        let mut first = sessions(&nodes, &slots, &image);
        let outcomes =
            update_fleet(&mut link, Some(&image), &mut first, std::future::pending(), |_| {})
                .await
                .expect("the first upload");
        assert_eq!(outcomes, [Outcome::Done(NODE_VERSION)]);
        // The cache answers a `Begin` for the image it holds `Verified`. An `Install` after it
        // would be refused, failing the upload. The node took the image on its new slot.
        let slots = HashMap::from([(nodes[0], slots[&nodes[0]])]);
        let mut again = sessions_from(&nodes, &slots, &image, NODE_VERSION);
        let outcomes =
            update_fleet(&mut link, Some(&image), &mut again, std::future::pending(), |_| {})
                .await
                .expect("the second upload");
        assert_eq!(outcomes, [Outcome::Done(NODE_VERSION)]);
    }

    #[tokio::test(start_paused = true)]
    async fn ota_cmd_sends_no_chunk_when_cache_has_not_verified() {
        let no_image = Arc::new(AtomicU32::new(0));
        let config = SimConfig {
            node_count: 4,
            speed: SPEED,
            fleet_version: minor_behind(NODE_VERSION),
            no_image_answers: Some(Arc::clone(&no_image)),
            ..SimConfig::default()
        };
        let mut link = SimTransport::new(config).start().expect("starting the simulator");
        connected(&mut link).await.expect("the simulator connects");
        let nodes: Vec<Mac> = (0..4).map(SimTransport::node_mac).collect();
        let (_, slots) = listen(&mut link, Duration::from_secs(1)).await.expect("listening");
        let image = sim_image(6);
        let mut sessions = sessions(&nodes, &slots, &image);
        let mut accepted_before_cached = 0;
        let mut cached = false;
        let report = |report: FleetReport<'_>| match report {
            FleetReport::Cached(_) => cached = true,
            FleetReport::Session(_, p) if !cached && p.phase == Phase::Streaming => {
                accepted_before_cached += 1;
            }
            FleetReport::Session(..) => {}
        };
        let outcomes =
            update_fleet(&mut link, Some(&image), &mut sessions, std::future::pending(), report)
                .await
                .expect("the upload");
        assert_eq!(outcomes, vec![Outcome::Done(NODE_VERSION); 4]);
        // The bridge answers `NoImage` to a chunk command from an image it has not verified.
        assert_eq!(no_image.load(Ordering::Relaxed), 0, "a chunk went before the cache verified");
        // The begins went beside the upload, not after it.
        assert!(accepted_before_cached > 0, "a node took its begin before the cache verified");
    }

    #[test]
    fn ota_cmd_runs_a_fleet_per_board_with_confirms_first_when_boards_differ() {
        let node = |i: u8| [0x02, 0, 0, 0, 0, i];
        let c5 = Capabilities::here(Board::Esp32c5);
        let other = Capabilities::here(Board::Unknown(9));
        let chosen = [
            (node(0), c5, false),
            (node(1), other, false),
            (node(2), c5, true),
            (node(3), c5, false),
        ];
        assert_eq!(
            fleets(&chosen),
            [(Some(Board::Esp32c5), vec![0, 3, 2]), (Some(Board::Unknown(9)), vec![1])]
        );
        assert_eq!(fleets(&[(node(0), c5, true)]), [(None, vec![0])]);
    }

    #[tokio::test(start_paused = true)]
    async fn ota_cmd_ends_the_fleet_when_bridge_loses_the_image_mid_run() {
        let config = SimConfig {
            node_count: 4,
            speed: SPEED,
            fleet_version: minor_behind(NODE_VERSION),
            drop_cache_after: Some(20),
            ..SimConfig::default()
        };
        let mut link = SimTransport::new(config).start().expect("starting the simulator");
        connected(&mut link).await.expect("the simulator connects");
        let nodes: Vec<Mac> = (0..4).map(SimTransport::node_mac).collect();
        let (_, slots) = listen(&mut link, Duration::from_secs(1)).await.expect("listening");
        let image = sim_image(6);
        let mut sessions = sessions(&nodes, &slots, &image);
        let ended =
            update_fleet(&mut link, Some(&image), &mut sessions, std::future::pending(), |_| {})
                .await;
        assert_eq!(ended, Err(FleetError::CacheLost));
        // One session met the `NoImage`, and the rest were cancelled, not left to meet it too.
        let outcomes: Vec<Outcome> = sessions.iter().filter_map(|s| s.outcome().cloned()).collect();
        let lost = outcomes.iter().filter(|o| **o == Outcome::Failed(Failure::CacheLost)).count();
        let cancelled = Outcome::Failed(Failure::Cancelled);
        assert_eq!((lost, outcomes.iter().filter(|o| **o == cancelled).count()), (1, 3));
        let error = verdict(&outcomes, Some(FleetError::CacheLost)).unwrap_err().to_string();
        assert!(error.starts_with("the bridge lost the node image;"), "{error}");
    }

    #[test]
    fn ota_cmd_counts_earlier_updates_when_a_later_upload_fails() {
        let cancelled = Outcome::Failed(Failure::Cancelled);
        let outcomes = [Outcome::Done(NEW), Outcome::Done(NEW), cancelled.clone()];
        let failed = Some(FleetError::Upload(update::Failure::Silent));
        let error = verdict(&outcomes, failed).unwrap_err().to_string();
        let expected = "the bridge did not take the node image (it stopped answering); 2 nodes \
                        were updated before it";
        assert_eq!(error, expected);
        let error = verdict(&[cancelled], failed).unwrap_err().to_string();
        assert!(error.ends_with("; nothing was updated"), "{error}");
    }
}
