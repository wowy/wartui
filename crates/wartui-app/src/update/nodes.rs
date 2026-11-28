//! The node update: every chosen node over the node bus, all at once, through the bridge.
//!
//! **It listens before it acts.** A node is reached through the slot its heartbeat came from,
//! and only a heartbeat says what board and version it is, so [`listen`] gathers them first, for
//! as long as the caller gives it: long enough for an assigned node, which beats every 5 s, to be
//! heard once. Only the heartbeat's frozen prefix is read, so a node a minor version behind,
//! which a capture cannot drive, is exactly the node this reaches.
//!
//! **The plan is the decision, and a front end shows it before anything is sent.** Each node's
//! [`Action`] follows from its version and whether there is an image for its board. A node on
//! trial is updated like any other: it confirms its running image on the first update frame it
//! decodes, before taking the update into its other slot. A [`Plan`] owns its images, so [`run`]
//! cannot be handed an image the plan was not decided against.
//!
//! **The image crosses the host link once.** Every node of a board takes the same image, so the
//! run uploads it into the bridge's node image cache, and each session names its chunks by
//! offset. Ten nodes cost one upload rather than ten copies of the image on the wire. The upload
//! runs beside the sessions' `begin`s ([`drive_fleet`]), so a node is ready to take chunks by the
//! time the bridge has checked the image, and none is sent one before. The bridge caches one
//! image, so a run with images for two boards updates one board's nodes, then the other's
//! (`fleets`).
//!
//! **Every chosen node updates at once**, one session each over the one link
//! ([`drive_fleet`]). The host link stops being the limit once the image is cached, so the two
//! buses side by side are what an update waits on. There is no canary: an image that boots and
//! misbehaves is caught in development, not here. Each session keeps every safety net of its own,
//! the trial boot and rollback included, so a node that fails leaves the others running and its
//! own old image in place. A link that drops mid-update fails each session as a pulled cable does.
//!
//! **Each session starts from the slot the listen heard its node in**, so its `begin` goes at
//! once rather than on the node's next heartbeat. Events that queued while the front end was busy
//! are thrown away before the sessions start: a heartbeat that waited out an image build or the
//! operator's answer is history. A node that moved since is found by its next heartbeat.
//!
//! **The scan stops before the first update record** ([`stop_scan`]), so the P4 sends the nodes
//! nothing of its own while they update, and the fleet stays idle afterwards until a run starts
//! it.
//!
//! **A stop cancels the sessions, so each node is told before the run ends.** One that comes
//! before any session starts ends the run with nothing sent ([`CutShort::BeforeStart`]).
//!
//! **A bridge this host cannot read past the frozen core is refused up front**
//! ([`CutShort::Incompatible`]): a node's update travels in messages outside that core.
//!
//! [`wartui_core::ota`] has the session's own reasoning.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use wartui_bridge::{BridgeCore, LinkEvent, LinkHandle};
use wartui_core::ota::{self, Failure, Ids, OtaImage, OtaSession, Outcome};
use wartui_core::runtime::{FleetError, FleetReport, drive_fleet, now, stop_scan};
use wartui_proto::bus::{Board, Capabilities, Heartbeat, Slot, UpType, Version};
use wartui_proto::link::BridgeToHost;
use wartui_proto::mac::Mac;
use wartui_proto::radio_link::update;

use super::image::{NodeImage, has_node_image};

/// How long a run waits for the bridge to report its scan stopped before updating anyway.
pub const SCAN_STOP_WAIT: Duration = Duration::from_secs(2);

/// What a listen heard: every node that heartbeated, as its latest heartbeat's prefix says, and
/// the slot that heartbeat came from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Heard {
    pub nodes: BTreeMap<Mac, Capabilities>,
    /// A slot names the node heard from it last: a node another node's heartbeat displaced keeps
    /// its place in `nodes`, with no slot.
    pub slots: HashMap<Mac, Slot>,
}

/// The link closed while a listen ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the link closed")]
pub struct LinkClosed;

/// Every node heartbeating within `period`, and the slot each was heard in.
///
/// # Errors
/// The link closed.
pub async fn listen(link: &mut LinkHandle, period: Duration) -> Result<Heard, LinkClosed> {
    let mut heard = Heard::default();
    let deadline = tokio::time::Instant::now() + period;
    loop {
        let event = match tokio::time::timeout_at(deadline, link.recv()).await {
            Err(_) => return Ok(heard),
            Ok(None) => return Err(LinkClosed),
            Ok(Some(event)) => event,
        };
        if let LinkEvent::Message(BridgeToHost::FromNode { slot, kind, body, .. }) = event
            && kind == UpType::Heartbeat.as_u8()
            && let Ok((caps, mac)) = Heartbeat::identity(&body)
        {
            heard.nodes.insert(mac, caps);
            heard.slots.retain(|_, held| *held != slot);
            heard.slots.insert(mac, slot);
        }
    }
}

/// A node an update may go to: what it said it is, and the slot it was heard in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub node: Mac,
    pub caps: Capabilities,
    pub slot: Option<Slot>,
}

/// Why no node was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SelectError {
    /// No node heartbeated.
    #[error("no node heard")]
    NoneHeard,
    /// No node heartbeated from this slot, which was named.
    #[error("no node heard in {0}")]
    NoneInSlot(Slot),
}

/// The nodes heard in the `named` slots, or every one heard when none is named, in address
/// order.
///
/// # Errors
/// No node was heard, or none in a slot named.
pub fn select(heard: &Heard, named: &[Slot]) -> Result<Vec<Target>, SelectError> {
    if heard.nodes.is_empty() {
        return Err(SelectError::NoneHeard);
    }
    let target = |(node, caps): (Mac, Capabilities)| Target {
        node,
        caps,
        slot: heard.slots.get(&node).copied(),
    };
    if named.is_empty() {
        return Ok(heard.nodes.iter().map(|(node, caps)| target((*node, *caps))).collect());
    }
    let mut chosen = BTreeMap::new();
    for slot in named {
        let Some((node, caps)) = heard
            .slots
            .iter()
            .find(|(_, held)| *held == slot)
            .and_then(|(node, _)| heard.nodes.get(node).map(|caps| (*node, *caps)))
        else {
            return Err(SelectError::NoneInSlot(*slot));
        };
        chosen.insert(node, caps);
    }
    Ok(chosen.into_iter().map(target).collect())
}

/// The boards among `targets` a node image exists for, each once.
///
/// One with none is left out, so the plan refuses that node and the run goes on. Asking for its
/// image would stop the update for every other node too.
#[must_use]
pub fn boards(targets: &[Target]) -> Vec<Board> {
    let mut boards: Vec<Board> = targets
        .iter()
        .map(|target| target.caps.board)
        .filter(|board| has_node_image(*board))
        .collect();
    boards.sort_by_key(|board| board.as_u8());
    boards.dedup();
    boards
}

/// What the run does with one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
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

/// One node of a plan: the node, the version of the image for its board, and what happens to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub target: Target,
    pub image: Option<Version>,
    pub action: Action,
}

/// What a run does with each node, and the image each board it updates takes.
#[derive(Debug, Clone)]
pub struct Plan {
    rows: Vec<Row>,
    images: HashMap<Board, NodeImage>,
}

impl Plan {
    /// Decide each of `targets` against the image for its board among `images`. `force` also
    /// updates a node already on the image's version, or on a newer one. An image is kept only
    /// under its own board, and only when a node takes it.
    #[must_use]
    pub fn of(targets: Vec<Target>, mut images: HashMap<Board, NodeImage>, force: bool) -> Self {
        images.retain(|board, image| image.board() == *board);
        let rows: Vec<Row> = targets
            .into_iter()
            .map(|target| {
                let image = images.get(&target.caps.board).map(NodeImage::version);
                Row { target, image, action: decide(&target.caps, image, force) }
            })
            .collect();
        images.retain(|board, _| {
            rows.iter().any(|row| row.action == Action::Update && row.target.caps.board == *board)
        });
        Self { rows, images }
    }

    /// Each node, in the order given.
    #[must_use]
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// How many nodes the run updates or confirms.
    #[must_use]
    pub fn chosen(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| matches!(row.action, Action::Update | Action::Confirm))
            .count()
    }
}

/// What a run reports as it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// The bridge did not report its scan stopped within [`SCAN_STOP_WAIT`]; the update goes on.
    ScanNotStopped,
    /// A fleet starts: its nodes, and the image's size when it sends one.
    Fleet { board: Option<Board>, nodes: Vec<Mac>, image_bytes: Option<usize> },
    /// The bridge checked the image in its cache, this long after the fleet started.
    Cached { took: Duration },
    /// Where a node's session stands.
    Node { node: Mac, progress: ota::Progress },
    /// The fleet ended: each node that has an outcome, with it.
    FleetEnded { outcomes: Vec<(Mac, Outcome)> },
}

/// How a run ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ended {
    /// Every node's that has one, in the order the fleets ran.
    pub outcomes: Vec<(Mac, Outcome)>,
    /// Why the run ended before every chosen node had its turn, if it did.
    pub cut_short: Option<CutShort>,
}

/// Why a run ended early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CutShort {
    /// `stop` resolved before any node was told anything.
    BeforeStart,
    /// The bridge refused or lost the upload into its cache.
    Upload(update::Failure),
    /// The bridge lost the image after checking it.
    CacheLost,
    /// The bridge shares only the frozen core with this host, which carries no node update.
    /// Nothing was sent.
    Incompatible(BridgeCore),
}

/// How a run that did not update every chosen node failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Failed {
    /// The bridge did not take the node image, after `done` nodes were updated.
    #[error("the bridge did not take the node image ({failure:?}); {done} done before it")]
    Upload { failure: update::Failure, done: usize },
    /// The bridge lost the node image.
    #[error("the bridge lost the node image")]
    CacheLost,
    /// The run was stopped.
    #[error("canceled")]
    Canceled,
    /// The bridge shares only the frozen core with this host, which carries no node update.
    #[error("the bridge {} shares only the frozen core with this host", .0.version)]
    Incompatible(BridgeCore),
    /// `failed` of `total` nodes did not update.
    #[error("{failed} of {total} nodes failed")]
    Nodes { failed: usize, total: usize },
}

impl Ended {
    /// `Ok` when every node with an outcome updated. A cut short upload names how many updated
    /// before it, and a stop or any canceled session is [`Failed::Canceled`].
    ///
    /// # Errors
    /// How the run failed.
    pub fn verdict(&self) -> Result<(), Failed> {
        let done =
            || self.outcomes.iter().filter(|(_, outcome)| matches!(outcome, Outcome::Done(_)));
        match self.cut_short {
            Some(CutShort::BeforeStart) => return Err(Failed::Canceled),
            Some(CutShort::Upload(failure)) => {
                return Err(Failed::Upload { failure, done: done().count() });
            }
            Some(CutShort::CacheLost) => return Err(Failed::CacheLost),
            Some(CutShort::Incompatible(core)) => return Err(Failed::Incompatible(core)),
            None => {}
        }
        if self.outcomes.iter().any(|(_, outcome)| *outcome == Outcome::Failed(Failure::Canceled)) {
            return Err(Failed::Canceled);
        }
        let failed = self.outcomes.len() - done().count();
        if failed > 0 {
            return Err(Failed::Nodes { failed, total: self.outcomes.len() });
        }
        Ok(())
    }
}

/// Update every node `plan` chooses, until each has an outcome or `stop` resolves, which
/// cancels every session. With nothing chosen, or a bridge this host cannot read
/// ([`CutShort::Incompatible`]), it returns at once and sends nothing.
pub async fn run(
    link: &mut LinkHandle,
    plan: &Plan,
    stop: impl Future<Output = ()>,
    say: impl FnMut(Progress),
) -> Ended {
    run_from(link, plan, session_base(), stop, say).await
}

/// The first session byte: from the clock, so a host restarted mid-update is unlikely to reuse
/// it; the node copes if it does.
fn session_base() -> u8 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| u8::try_from(since.as_secs() % 256).ok())
        .unwrap_or(1)
}

/// [`run`], with session `i` of the run named `base + i` on the wire.
async fn run_from(
    link: &mut LinkHandle,
    plan: &Plan,
    base: u8,
    stop: impl Future<Output = ()>,
    mut say: impl FnMut(Progress),
) -> Ended {
    if let Some(core) = link.incompatible() {
        return Ended { outcomes: Vec::new(), cut_short: Some(CutShort::Incompatible(core)) };
    }
    let chosen: Vec<(Target, bool)> = plan
        .rows
        .iter()
        .filter_map(|row| match row.action {
            Action::Update => Some((row.target, false)),
            Action::Confirm => Some((row.target, true)),
            _ => None,
        })
        .collect();
    if chosen.is_empty() {
        return Ended::default();
    }
    tokio::pin!(stop);
    // Stop the scan first, so the P4 sends the nodes nothing of its own while they update.
    tokio::select! {
        stopped = stop_scan(link, SCAN_STOP_WAIT) => {
            if !stopped {
                say(Progress::ScanNotStopped);
            }
        }
        () = &mut stop => {
            return Ended { outcomes: Vec::new(), cut_short: Some(CutShort::BeforeStart) };
        }
    }

    let mut outcomes = Vec::new();
    for (board, members) in fleets(&chosen) {
        let image = board.map(|board| plan.images[&board].ota());
        let mut sessions: Vec<OtaSession> = members
            .iter()
            .map(|&i| {
                let (target, confirm_only) = chosen[i];
                let ids = Ids::of(i, chosen.len());
                match &image {
                    Some(image) if !confirm_only => {
                        let session = base.wrapping_add(u8::try_from(i).unwrap_or(0));
                        let version = target.caps.version;
                        let session =
                            OtaSession::new(target.node, version, image, session, ids, now());
                        match target.slot {
                            Some(slot) => session.with_slot(slot),
                            None => session,
                        }
                    }
                    _ => OtaSession::confirm_trial(target.node, target.caps.version, ids, now()),
                }
            })
            .collect();
        let nodes: Vec<Mac> = members.iter().map(|&i| chosen[i].0.node).collect();
        let image_bytes = image.as_ref().map(|image| image.bytes.len());
        say(Progress::Fleet { board, nodes: nodes.clone(), image_bytes });
        let report = |report: FleetReport<'_>| match report {
            FleetReport::Cached(took) => say(Progress::Cached { took }),
            FleetReport::Session(i, progress) => {
                if let Some(node) = nodes.get(i) {
                    say(Progress::Node { node: *node, progress: *progress });
                }
            }
        };
        let ended = update_fleet(link, image.as_ref(), &mut sessions, &mut stop, report).await;
        // A fleet the bridge stopped still reports which nodes finished before it did.
        let (got, cut) = match ended {
            Ok(got) => (nodes.iter().copied().zip(got).collect::<Vec<_>>(), None),
            Err(error) => {
                let got = sessions.iter().filter_map(|session| {
                    session.outcome().map(|outcome| (session.node(), outcome.clone()))
                });
                (got.collect(), Some(error))
            }
        };
        say(Progress::FleetEnded { outcomes: got.clone() });
        outcomes.extend(got);
        if let Some(error) = cut {
            let cut_short = match error {
                FleetError::Upload(failure) => CutShort::Upload(failure),
                FleetError::CacheLost => CutShort::CacheLost,
            };
            return Ended { outcomes, cut_short: Some(cut_short) };
        }
        if outcomes.iter().any(|(_, outcome)| *outcome == Outcome::Failed(Failure::Canceled)) {
            break;
        }
    }
    Ended { outcomes, cut_short: None }
}

/// The chosen nodes as the fleets that run one after another, as indices into `chosen`: one per
/// board with an image to send, since the bridge caches one image, and the confirms with the
/// first. A fleet of confirms alone sends no image. Each node is `true` when only confirmed.
fn fleets(chosen: &[(Target, bool)]) -> Vec<(Option<Board>, Vec<usize>)> {
    let mut fleets: Vec<(Option<Board>, Vec<usize>)> = Vec::new();
    for (i, (target, confirm_only)) in chosen.iter().enumerate() {
        if *confirm_only {
            continue;
        }
        match fleets.iter_mut().find(|(board, _)| *board == Some(target.caps.board)) {
            Some((_, members)) => members.push(i),
            None => fleets.push((Some(target.caps.board), vec![i])),
        }
    }
    let confirms = chosen.iter().enumerate().filter(|(_, (_, confirm))| *confirm).map(|(i, _)| i);
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

/// Throw away whatever queued while the front end was doing something else.
async fn drain(link: &mut LinkHandle) {
    while let Ok(Some(_)) = tokio::time::timeout(Duration::ZERO, link.recv()).await {}
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use wartui_bridge::link_pair;
    use wartui_bridge::sim::{SimConfig, SimTransport, minor_behind};
    use wartui_proto::bus::{NODE_VERSION, UpdateState, UpdateStatus};
    use wartui_proto::link::{BRIDGE_VERSION, HostToBridge, Standing, Trial};

    use super::super::testing::fixture;
    use super::*;
    use crate::ops;
    use crate::settings::Link;

    const NEW: Version = Version::new(0, 1, 1);

    fn node(i: u8) -> Mac {
        [0x02, 0, 0, 0, 0, i]
    }

    fn caps(version: Version, trial: bool) -> Capabilities {
        Capabilities { version, trial, ..Capabilities::here(Board::Esp32c5) }
    }

    fn target(node: Mac, caps: Capabilities) -> Target {
        Target { node, caps, slot: None }
    }

    #[test]
    fn update_nodes_updates_node_when_image_is_newer() {
        let node = caps(Version::new(0, 0, 9), false);
        assert_eq!(decide(&node, Some(NEW), false), Action::Update);
    }

    #[test]
    fn update_nodes_skips_node_when_already_on_image_version() {
        assert_eq!(decide(&caps(NEW, false), Some(NEW), false), Action::Same(NEW));
        assert_eq!(decide(&caps(NEW, false), Some(NEW), true), Action::Update);
    }

    #[test]
    fn update_nodes_skips_node_when_image_is_older() {
        assert_eq!(decide(&caps(Version::new(0, 2, 0), false), Some(NEW), false), Action::Older);
        assert_eq!(decide(&caps(Version::new(0, 2, 0), false), Some(NEW), true), Action::Update);
    }

    #[test]
    fn update_nodes_refuses_node_when_no_image_for_board() {
        let node = Capabilities::here(Board::Esp32c6);
        assert_eq!(decide(&node, None, true), Action::NoImage(Board::Esp32c6));
    }

    #[test]
    fn update_nodes_skips_board_when_it_has_no_node_image() {
        // One leftover C6 must not stop the update for every C5 beside it.
        let targets = [
            target(node(1), Capabilities::here(Board::Esp32c6)),
            target(node(2), Capabilities::here(Board::Esp32c5)),
            target(node(3), Capabilities::here(Board::Unknown(3))),
            target(node(4), Capabilities::here(Board::Esp32c5)),
        ];
        assert_eq!(boards(&targets), [Board::Esp32c5]);
    }

    #[test]
    fn update_nodes_confirms_node_when_it_runs_the_image_version_on_trial() {
        assert_eq!(decide(&caps(NEW, true), Some(NEW), false), Action::Confirm);
        // Not sent again, even when forced.
        assert_eq!(decide(&caps(NEW, true), Some(NEW), true), Action::Confirm);
    }

    #[test]
    fn update_nodes_updates_node_when_it_runs_a_trial_image() {
        // The node confirms its running image on the update's first frame.
        let action = decide(&caps(Version::new(0, 0, 9), true), Some(NEW), false);
        assert_eq!(action, Action::Update);
    }

    #[test]
    fn update_nodes_plans_each_node_when_images_are_given() {
        let image = NodeImage::parse(fixture(Board::Esp32c5, "0.1.1", 0x1000)).unwrap();
        let targets = vec![
            target(node(1), caps(Version::new(0, 0, 9), false)),
            target(node(2), caps(NEW, false)),
            target(node(3), caps(NEW, true)),
            target(node(4), Capabilities::here(Board::Esp32c6)),
        ];
        let plan = Plan::of(targets, HashMap::from([(Board::Esp32c5, image)]), false);
        let actions: Vec<Action> = plan.rows().iter().map(|row| row.action).collect();
        let expected =
            [Action::Update, Action::Same(NEW), Action::Confirm, Action::NoImage(Board::Esp32c6)];
        assert_eq!(actions, expected);
        assert_eq!(plan.rows()[0].image, Some(NEW));
        assert_eq!(plan.rows()[3].image, None);
        assert_eq!(plan.chosen(), 2);
    }

    #[test]
    fn update_nodes_ignores_image_when_filed_under_another_board() {
        let image = NodeImage::parse(fixture(Board::Esp32c5, "0.1.1", 0x1000)).unwrap();
        let targets = vec![target(node(1), Capabilities::here(Board::Esp32c6))];
        let plan = Plan::of(targets, HashMap::from([(Board::Esp32c6, image)]), true);
        assert_eq!(plan.rows()[0].action, Action::NoImage(Board::Esp32c6));
        assert!(plan.images.is_empty());
    }

    /// A heard fleet: node 0 in A1 and node 1 in A2, and node 2 displaced from A1 by node 0.
    fn heard() -> Heard {
        let a1 = Slot::new(0, 0).unwrap();
        let a2 = Slot::new(0, 1).unwrap();
        Heard {
            nodes: (0..3).map(|i| (node(i), caps(NEW, false))).collect(),
            slots: HashMap::from([(node(0), a1), (node(1), a2)]),
        }
    }

    #[test]
    fn update_nodes_selects_every_node_heard_when_none_is_named() {
        let chosen = select(&heard(), &[]).unwrap();
        let nodes: Vec<Mac> = chosen.iter().map(|target| target.node).collect();
        assert_eq!(nodes, [node(0), node(1), node(2)]);
        assert_eq!(chosen[0].slot, Slot::new(0, 0));
        assert_eq!(chosen[2].slot, None, "displaced");
    }

    #[test]
    fn update_nodes_selects_nodes_by_slot_when_named() {
        let a2 = Slot::new(0, 1).unwrap();
        let chosen = select(&heard(), &[a2]).unwrap();
        assert_eq!(chosen, [Target { node: node(1), caps: caps(NEW, false), slot: Some(a2) }]);
    }

    #[test]
    fn update_nodes_refuses_selection_when_no_node_is_in_a_named_slot() {
        let a3 = Slot::new(0, 2).unwrap();
        assert_eq!(select(&heard(), &[a3]), Err(SelectError::NoneInSlot(a3)));
    }

    #[test]
    fn update_nodes_refuses_selection_when_no_node_is_heard() {
        assert_eq!(select(&Heard::default(), &[]), Err(SelectError::NoneHeard));
    }

    #[test]
    fn update_nodes_runs_a_fleet_per_board_with_confirms_first_when_boards_differ() {
        let c5 = Capabilities::here(Board::Esp32c5);
        let other = Capabilities::here(Board::Unknown(9));
        let chosen = [
            (target(node(0), c5), false),
            (target(node(1), other), false),
            (target(node(2), c5), true),
            (target(node(3), c5), false),
        ];
        assert_eq!(
            fleets(&chosen),
            [(Some(Board::Esp32c5), vec![0, 3, 2]), (Some(Board::Unknown(9)), vec![1])]
        );
        assert_eq!(fleets(&[(target(node(0), c5), true)]), [(None, vec![0])]);
    }

    fn ended(outcomes: Vec<Outcome>, cut_short: Option<CutShort>) -> Ended {
        let outcomes = outcomes.into_iter().zip(0..).map(|(o, i)| (node(i), o)).collect();
        Ended { outcomes, cut_short }
    }

    #[test]
    fn update_nodes_counts_earlier_updates_when_a_later_upload_fails() {
        let canceled = Outcome::Failed(Failure::Canceled);
        let failure = update::Failure::Silent;
        let outcomes = vec![Outcome::Done(NEW), Outcome::Done(NEW), canceled.clone()];
        let verdict = ended(outcomes, Some(CutShort::Upload(failure))).verdict();
        assert_eq!(verdict, Err(Failed::Upload { failure, done: 2 }));
        let verdict = ended(vec![canceled], Some(CutShort::Upload(failure))).verdict();
        assert_eq!(verdict, Err(Failed::Upload { failure, done: 0 }));
    }

    #[test]
    fn update_nodes_fails_cache_lost_when_bridge_lost_the_image() {
        let verdict = ended(vec![Outcome::Done(NEW)], Some(CutShort::CacheLost)).verdict();
        assert_eq!(verdict, Err(Failed::CacheLost));
    }

    #[test]
    fn update_nodes_fails_canceled_when_stopped_before_start_or_a_session_was_canceled() {
        assert_eq!(ended(vec![], Some(CutShort::BeforeStart)).verdict(), Err(Failed::Canceled));
        let outcomes = vec![Outcome::Done(NEW), Outcome::Failed(Failure::Canceled)];
        assert_eq!(ended(outcomes, None).verdict(), Err(Failed::Canceled));
    }

    #[test]
    fn update_nodes_counts_failed_nodes_when_some_did_not_update() {
        let outcomes = vec![Outcome::Done(NEW), Outcome::Failed(Failure::NoAnswer)];
        assert_eq!(ended(outcomes, None).verdict(), Err(Failed::Nodes { failed: 1, total: 2 }));
        assert_eq!(ended(vec![Outcome::Done(NEW); 3], None).verdict(), Ok(()));
    }

    /// How much faster than real time the fake fleet runs.
    const SPEED: f64 = 20.0;

    /// A simulated fleet as `config` describes it, connected, and every node heard.
    async fn sim(config: SimConfig) -> (LinkHandle, Heard) {
        let count = config.node_count;
        let mut link = SimTransport::new(config).start().expect("starting the simulator");
        let wait = Duration::from_secs(5);
        let waited = ops::ready(&mut link, Link::Usb, wait, wait, &mut |_| {}).await;
        waited.ready.expect("the simulator connects").expect("the simulator announces itself");
        let heard = listen(&mut link, Duration::from_secs(1)).await.expect("listening");
        let nodes: Vec<Mac> = (0..count).map(SimTransport::node_mac).collect();
        assert!(nodes.iter().all(|node| heard.nodes.contains_key(node)), "every node heard");
        (link, heard)
    }

    /// A simulated fleet of `count` nodes a minor behind, `rejecting` the image on one.
    fn sim_config(count: u8, rejecting: Option<u8>) -> SimConfig {
        SimConfig {
            node_count: count,
            speed: SPEED,
            fleet_version: minor_behind(NODE_VERSION),
            rejecting_node: rejecting,
            ..SimConfig::default()
        }
    }

    /// An image of `sectors` sectors and a bit, for [`NODE_VERSION`].
    fn sim_image(sectors: usize) -> NodeImage {
        let len = (sectors * 4096 + 700).next_multiple_of(16);
        NodeImage::parse(fixture(Board::Esp32c5, &NODE_VERSION.to_string(), len)).unwrap()
    }

    /// A plan updating every node heard to `image`.
    fn plan(heard: &Heard, image: NodeImage, force: bool) -> Plan {
        let targets = select(heard, &[]).unwrap();
        Plan::of(targets, HashMap::from([(Board::Esp32c5, image)]), force)
    }

    /// The outcomes in node order.
    fn outcomes(ended: &Ended) -> Vec<Outcome> {
        ended.outcomes.iter().map(|(_, outcome)| outcome.clone()).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn update_nodes_updates_every_node_when_nothing_is_lost() {
        let (mut link, heard) = sim(sim_config(4, None)).await;
        let plan = plan(&heard, sim_image(6), false);
        let ended = run(&mut link, &plan, std::future::pending(), |_| {}).await;
        assert_eq!(outcomes(&ended), vec![Outcome::Done(NODE_VERSION); 4]);
        assert_eq!(ended.cut_short, None);
        assert_eq!(ended.verdict(), Ok(()));
    }

    #[tokio::test(start_paused = true)]
    async fn update_nodes_finishes_the_rest_when_one_node_rolls_back() {
        let (mut link, heard) = sim(sim_config(4, Some(2))).await;
        let plan = plan(&heard, sim_image(6), false);
        let ended = run(&mut link, &plan, std::future::pending(), |_| {}).await;
        let got = outcomes(&ended);
        let old = minor_behind(NODE_VERSION);
        assert_eq!(got[2], Outcome::Failed(Failure::RolledBack(old)));
        for i in [0, 1, 3] {
            assert_eq!(got[i], Outcome::Done(NODE_VERSION), "node {i}");
        }
        assert_eq!(ended.verdict(), Err(Failed::Nodes { failed: 1, total: 4 }));
    }

    #[tokio::test(start_paused = true)]
    async fn update_nodes_sends_one_abort_per_session_when_canceled() {
        let (mut link, heard) = sim(sim_config(4, None)).await;
        // Long enough that every node is still streaming when the cancel lands.
        let plan = plan(&heard, sim_image(128), false);
        let nodes: Vec<Mac> = plan.rows().iter().map(|row| row.target.node).collect();
        let streaming = tokio::sync::Notify::new();
        let mut seen = vec![false; nodes.len()];
        let say = |progress: Progress| {
            if let Progress::Node { node, progress } = progress
                && let Some(i) = nodes.iter().position(|n| *n == node)
            {
                seen[i] |= progress.phase == ota::Phase::Streaming && progress.acked > 0;
            }
            if seen.iter().all(|s| *s) {
                streaming.notify_one();
            }
        };
        let ended = run_from(&mut link, &plan, 0x40, streaming.notified(), say).await;
        assert_eq!(outcomes(&ended), vec![Outcome::Failed(Failure::Canceled); 4]);
        assert_eq!(ended.verdict(), Err(Failed::Canceled));
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
    async fn update_nodes_updates_node_when_cache_already_holds_the_image() {
        let (mut link, heard) = sim(sim_config(1, None)).await;
        let image = sim_image(3);
        let first = plan(&heard, image.clone(), false);
        let ended = run(&mut link, &first, std::future::pending(), |_| {}).await;
        assert_eq!(outcomes(&ended), [Outcome::Done(NODE_VERSION)]);
        // The cache answers a `Begin` for the image it holds `Verified`. An `Install` after it
        // would be refused, failing the upload. The node took the image on its new slot.
        let mut again = first.rows()[0].target;
        again.caps.version = NODE_VERSION;
        let again = Plan::of(vec![again], HashMap::from([(Board::Esp32c5, image)]), true);
        let ended = run(&mut link, &again, std::future::pending(), |_| {}).await;
        assert_eq!(outcomes(&ended), [Outcome::Done(NODE_VERSION)]);
    }

    #[tokio::test(start_paused = true)]
    async fn update_nodes_sends_no_chunk_when_cache_has_not_verified() {
        let no_image = Arc::new(AtomicU32::new(0));
        let config =
            SimConfig { no_image_answers: Some(Arc::clone(&no_image)), ..sim_config(4, None) };
        let (mut link, heard) = sim(config).await;
        let plan = plan(&heard, sim_image(6), false);
        let mut accepted_before_cached = 0;
        let mut cached = false;
        let say = |progress: Progress| match progress {
            Progress::Cached { .. } => cached = true,
            Progress::Node { progress, .. }
                if !cached && progress.phase == ota::Phase::Streaming =>
            {
                accepted_before_cached += 1;
            }
            _ => {}
        };
        let ended = run(&mut link, &plan, std::future::pending(), say).await;
        assert_eq!(outcomes(&ended), vec![Outcome::Done(NODE_VERSION); 4]);
        // The bridge answers `NoImage` to a chunk command from an image it has not verified.
        assert_eq!(no_image.load(Ordering::Relaxed), 0, "a chunk went before the cache verified");
        // The begins went beside the upload, not after it.
        assert!(accepted_before_cached > 0, "a node took its begin before the cache verified");
    }

    #[tokio::test(start_paused = true)]
    async fn update_nodes_ends_the_fleet_when_bridge_loses_the_image_mid_run() {
        let config = SimConfig { drop_cache_after: Some(20), ..sim_config(4, None) };
        let (mut link, heard) = sim(config).await;
        let plan = plan(&heard, sim_image(6), false);
        let ended = run(&mut link, &plan, std::future::pending(), |_| {}).await;
        assert_eq!(ended.cut_short, Some(CutShort::CacheLost));
        // One session met the `NoImage`, and the rest were canceled, not left to meet it too.
        let got = outcomes(&ended);
        let lost = got.iter().filter(|o| **o == Outcome::Failed(Failure::CacheLost)).count();
        let canceled = Outcome::Failed(Failure::Canceled);
        assert_eq!((lost, got.iter().filter(|o| **o == canceled).count()), (1, 3));
        assert_eq!(ended.verdict(), Err(Failed::CacheLost));
    }

    #[tokio::test(start_paused = true)]
    async fn update_nodes_says_each_fleet_and_its_outcomes_when_run() {
        let (mut link, heard) = sim(sim_config(2, None)).await;
        let image = sim_image(3);
        let len = image.bytes().len();
        let plan = plan(&heard, image, false);
        let mut said = Vec::new();
        let ended = run(&mut link, &plan, std::future::pending(), |progress| match progress {
            Progress::Node { .. } => {}
            progress => said.push(progress),
        })
        .await;
        let nodes: Vec<Mac> = (0..2).map(SimTransport::node_mac).collect();
        let fleet = Progress::Fleet { board: Some(Board::Esp32c5), nodes, image_bytes: Some(len) };
        assert_eq!(said[0], fleet);
        assert!(matches!(said[1], Progress::Cached { .. }), "{said:?}");
        assert_eq!(said[2], Progress::FleetEnded { outcomes: ended.outcomes.clone() });
        assert_eq!(said.len(), 3, "{said:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn update_nodes_sends_nothing_when_nothing_is_chosen() {
        let (mut link, mut plumbing) = link_pair();
        let image = NodeImage::parse(fixture(Board::Esp32c5, "0.1.1", 0x1000)).unwrap();
        let targets = vec![target(node(1), caps(NEW, false))];
        let plan = Plan::of(targets, HashMap::from([(Board::Esp32c5, image)]), false);
        assert_eq!(plan.chosen(), 0);
        let ended = run(&mut link, &plan, std::future::pending(), |_| panic!("said")).await;
        assert_eq!(ended, Ended::default());
        assert!(plumbing.commands.try_recv().is_none(), "the scan was not stopped either");
    }

    #[tokio::test(start_paused = true)]
    async fn update_nodes_refuses_when_bridge_is_incompatible() {
        let (mut link, mut plumbing) = link_pair();
        let version = Version::new(BRIDGE_VERSION.major + 1, 0, 0);
        let core = BridgeCore {
            mac: [0x02, 0x00, 0x5E, 0x10, 0x44, 0xC0],
            image: None,
            version,
            trial: Trial::Settled,
            uptime_ms: 400,
            standing: Standing::of(version, BRIDGE_VERSION, false),
        };
        plumbing.compatibility.heard(&core);
        let image = NodeImage::parse(fixture(Board::Esp32c5, "0.1.1", 0x1000)).unwrap();
        let targets = vec![target(node(1), caps(Version::new(0, 0, 9), false))];
        let plan = Plan::of(targets, HashMap::from([(Board::Esp32c5, image)]), false);
        let ended = run(&mut link, &plan, std::future::pending(), |_| panic!("said")).await;
        assert_eq!(ended.cut_short, Some(CutShort::Incompatible(core)));
        assert_eq!(ended.verdict(), Err(Failed::Incompatible(core)));
        assert!(
            plumbing.commands.try_recv().is_none(),
            "nothing was sent, the scan's stop included"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn update_nodes_returns_before_start_when_stop_has_resolved() {
        let (mut link, mut plumbing) = link_pair();
        let image = NodeImage::parse(fixture(Board::Esp32c5, "0.1.1", 0x1000)).unwrap();
        let targets = vec![target(node(1), caps(Version::new(0, 0, 9), false))];
        let plan = Plan::of(targets, HashMap::from([(Board::Esp32c5, image)]), false);
        let ended = run(&mut link, &plan, std::future::ready(()), |_| {}).await;
        let expected = Ended { outcomes: Vec::new(), cut_short: Some(CutShort::BeforeStart) };
        assert_eq!(ended, expected);
        // At most the scan's stop went, and no update record.
        while let Some(command) = plumbing.commands.try_recv() {
            assert!(
                matches!(command, HostToBridge::StopScan | HostToBridge::GetStatus),
                "{command:?}"
            );
        }
    }
}
