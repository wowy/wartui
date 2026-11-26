//! The nodes' and the bridge's firmware updated over a [`Link`]: `wartui_app::update`, for the
//! phone.
//!
//! **Nothing unchecked is sent.** A [`NodeImage`] or a [`BridgeImage`] exists only once its bytes
//! have passed every check the app makes (`wartui_app::update::image`), and a release's image is
//! checked against the release's `SHA256SUMS` before it is kept (`release`). The tag is the
//! phone's to give: the release its app is built against.
//!
//! **No consent value.** An upload cannot be taken back, so it takes one; an update can, since
//! each image boots on trial and rolls back when it does not prove itself (`wartui_app::update`
//! `//!`). What stands for the operator's yes is that [`NodePlan`] is opaque: the plan a confirm
//! screen shows is the plan [`Link::update_nodes`] runs.
//!
//! **Only a pinned link takes an update.** Over Bluetooth an update goes to the bridge named when
//! the link was opened, whose transport refuses every other through each reconnect, never to one
//! a scan happens to hear, whose fleet would take the image (`link.rs`). A simulated link is
//! exempt.
//!
//! **Each call works over the link as [`Link::ready`] does.** The link must be idle; it is lent
//! for the call, the work runs whole on the library's runtime, and it is given back parked on
//! every way out, a phone that stops awaiting included. A node update keeps the announcement it
//! was lent with. A bridge update clears it, whatever the outcome: the bridge may have restarted
//! into another image, and the next `ready` or capture reads it afresh.
//!
//! **A plan holds its images by board.** `wartui_app::update::image::NodeImage` shares its bytes
//! behind an `Arc`, so a [`NodeImage`] gives each plan made from it a clone of that handle, never
//! a copy of the image.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use wartui_app::ops;
use wartui_app::settings::Link as Over;
use wartui_app::update::bridge::{self, Failed, Line, Proof, Updated};
use wartui_app::update::image::{
    self as app_image, Chip, ImageError as AppImageError, ImageOf, has_node_image,
};
use wartui_app::update::nodes::{
    self, Action, CutShort as AppCutShort, Ended, Failed as AppFailed, Heard, LinkClosed, Plan,
    Row, SelectError, Target,
};
use wartui_app::update::release::{self, BRIDGE_APP_ASSET, ReleaseError, Releases};
use wartui_app::view::SlotShown;
use wartui_bridge::SendError;
use wartui_core::ota::{self, Failure, Outcome, Phase};
use wartui_proto::bus::{Board, Capabilities, NodeBoard, Slot, UpdateState};
use wartui_proto::mac::{self, Mac};
use wartui_proto::radio_link::update::Failure as AppSenderFailure;

use crate::link::{Inner, Link, LinkError};
use crate::log::SinkError;
use crate::runtime::{runtime, spawn};
use crate::settings::chain;

/// A chip the firmwares are built for.
#[uniffi::remote(Enum)]
pub enum Chip {
    Esp32c5,
    Esp32c6,
    Esp32p4,
}

/// Which firmware an image was checked as.
#[uniffi::remote(Enum)]
pub enum ImageOf {
    /// A node's.
    Node,
    /// The P4 bridge's.
    Bridge,
}

/// What a receiver of an update reports of it.
#[uniffi::remote(Enum)]
pub enum UpdateState {
    /// No session, or the session named is not the current one.
    Idle,
    /// Taking chunks.
    Receiving,
    /// The hash matched, and the receiver is about to switch slots and reboot.
    Verified,
    /// Every byte arrived and the hash did not match.
    HashMismatch,
    /// The image was built for another board.
    WrongBoard,
    /// The image is empty or larger than the slot.
    TooLarge,
    /// Erasing or writing the slot failed.
    FlashError,
    /// A byte this build does not name.
    Unknown(u8),
}

/// Where a node's update stands.
#[uniffi::remote(Enum)]
pub enum Phase {
    /// Sending `begin` on each heartbeat until the node answers.
    Begin,
    /// Sending chunks.
    Streaming,
    /// Every byte is with the node; waiting for it to check the hash.
    Committing,
    /// The image verified; waiting for the node to boot it.
    AwaitReboot,
    /// The new image is running on trial; confirming it.
    AwaitConfirm,
    /// Confirmed on the new image.
    Done,
    /// Given up.
    Failed,
}

/// A line the bridge sent while it updated.
#[uniffi::remote(Enum)]
pub enum Line {
    /// An `Error` message.
    Fault(String),
    /// A `Log` message.
    Log(String),
}

/// How the bridge's new image is known to be confirmed.
#[uniffi::remote(Enum)]
pub enum Proof {
    /// The P4's line said so.
    Confirmed,
    /// The line was lost, and the bridge outlived its trial watchdog.
    OutlivedTrial,
}

/// Why an image was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum ImageError {
    /// Fewer bytes than an image header.
    #[error("too short for an image header")]
    TooShort,
    /// No image header at byte 0.
    #[error("no image header at the start")]
    NoHeader,
    /// No image header past the erased flash ahead of it.
    #[error("no image header where a bootloader starts")]
    NoBootloaderHeader,
    /// The header names chip ID `id`, which is not `chip`.
    #[error("chip ID {id}, not the chip expected")]
    OtherChip { id: u16, chip: Chip },
    /// The header is at byte `at`, and an app-only image starts with it.
    #[error("header at {at}, not at the start")]
    NotAtStart { at: u64 },
    /// A merged image: bootloader, partition table and app.
    #[error("a merged image")]
    Merged,
    /// The first segment holds no app descriptor.
    #[error("no app descriptor")]
    NoDescriptor,
    /// The descriptor names `project`, which is not the firmware `of`'s.
    #[error("project {project}, not the firmware expected")]
    Project { project: String, of: ImageOf },
    /// The descriptor's version is not `major.minor.patch`.
    #[error("version {version} is not x.y.z")]
    Version { version: String },
    /// The header names chip ID `id`, and `project` is not built for that chip.
    #[error("chip ID {id} is not {project}'s")]
    ChipOfProject { id: u16, project: String },
    /// Segment `segment` has no header.
    #[error("truncated: segment {segment} has no header")]
    SegmentHeader { segment: u8 },
    /// Segment `segment` runs past the end of the file.
    #[error("truncated: segment {segment} runs past the end")]
    SegmentPastEnd { segment: u8 },
    /// No checksum byte after the segments.
    #[error("truncated: no checksum")]
    NoChecksum,
    /// The checksum byte is `stored`, and the segments give `computed`.
    #[error("checksum {stored}, segments give {computed}")]
    Checksum { stored: u8, computed: u8 },
    /// The header says a SHA-256 is appended, and none follows the checksum.
    #[error("truncated: no SHA-256")]
    NoSha256,
    /// The appended SHA-256 is not the contents'.
    #[error("appended SHA-256 does not match")]
    Sha256,
    /// `len` bytes, more than the app slot of the firmware `of` holds.
    #[error("{len} bytes, past the app slot")]
    TooLarge { len: u64, of: ImageOf },
    /// No appended digest, which names the image to the P4.
    #[error("no appended digest")]
    NoAppendedDigest,
}

impl From<AppImageError> for ImageError {
    fn from(error: AppImageError) -> Self {
        match error {
            AppImageError::TooShort => Self::TooShort,
            AppImageError::NoHeader => Self::NoHeader,
            AppImageError::NoBootloaderHeader => Self::NoBootloaderHeader,
            AppImageError::OtherChip { id, chip } => Self::OtherChip { id, chip },
            AppImageError::NotAtStart { at } => Self::NotAtStart { at: wide(at) },
            AppImageError::Merged => Self::Merged,
            AppImageError::NoDescriptor => Self::NoDescriptor,
            AppImageError::Project { project, of } => Self::Project { project, of },
            AppImageError::Version { version } => Self::Version { version },
            AppImageError::ChipOfProject { id, project } => Self::ChipOfProject { id, project },
            AppImageError::SegmentHeader { segment } => Self::SegmentHeader { segment },
            AppImageError::SegmentPastEnd { segment } => Self::SegmentPastEnd { segment },
            AppImageError::NoChecksum => Self::NoChecksum,
            AppImageError::Checksum { stored, computed } => Self::Checksum { stored, computed },
            AppImageError::NoSha256 => Self::NoSha256,
            AppImageError::Sha256 => Self::Sha256,
            AppImageError::TooLarge { len, of } => Self::TooLarge { len: wide(len), of },
            AppImageError::NoAppendedDigest => Self::NoAppendedDigest,
        }
    }
}

/// `n` as a `u64`, saturating.
fn wide(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// `bytes` as lowercase hex.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A node's app-only image, checked.
#[derive(Debug, uniffi::Object)]
pub struct NodeImage {
    image: app_image::NodeImage,
}

#[uniffi::export]
impl NodeImage {
    /// Check `bytes` as a node's app-only image. Nothing unchecked is ever sent.
    ///
    /// # Errors
    /// The first check the bytes fail.
    #[uniffi::constructor]
    pub fn parse(bytes: Vec<u8>) -> Result<Arc<Self>, ImageError> {
        Ok(Arc::new(Self { image: app_image::NodeImage::parse(bytes)? }))
    }

    /// The node firmware version it carries, `0.7.0`.
    #[must_use]
    pub fn version(&self) -> String {
        self.image.version().to_string()
    }

    /// The board it was built for.
    #[must_use]
    pub fn board(&self) -> Board {
        self.image.board()
    }

    /// Its SHA-256, as lowercase hex.
    #[must_use]
    pub fn sha256(&self) -> String {
        hex(&self.image.sha256())
    }
}

/// The P4 bridge's app-only image, checked.
#[derive(Debug, uniffi::Object)]
pub struct BridgeImage {
    image: app_image::BridgeImage,
}

#[uniffi::export]
impl BridgeImage {
    /// Check `bytes` as the P4 bridge's app-only image. Nothing unchecked is ever sent.
    ///
    /// # Errors
    /// The first check the bytes fail.
    #[uniffi::constructor]
    pub fn parse(bytes: Vec<u8>) -> Result<Arc<Self>, ImageError> {
        Ok(Arc::new(Self { image: app_image::BridgeImage::parse(bytes)? }))
    }

    /// Its SHA-256, as lowercase hex.
    #[must_use]
    pub fn sha256(&self) -> String {
        hex(&self.image.sha256())
    }
}

/// Why a release asset could not be had. `detail` is the OS's own account.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum ReleaseFailure {
    /// The request for `url` failed, in the HTTP client's words.
    Fetch { url: String, error: String },
    /// The answer from `url` could not be read, in the HTTP client's words.
    Read { url: String, error: String },
    /// `SHA256SUMS` is not UTF-8.
    SumsNotText { detail: String },
    /// `SHA256SUMS` lists no digest for `asset`.
    Unlisted { asset: String },
    /// `asset`'s digest is `actual`, and `SHA256SUMS` lists `listed`.
    Mismatch { asset: String, actual: String, listed: String },
    /// The cache directory `dir` could not be created.
    CreateDir { dir: String, detail: String },
    /// `path` could not be written into the cache.
    Write { path: String, detail: String },
}

impl From<ReleaseError> for ReleaseFailure {
    fn from(error: ReleaseError) -> Self {
        match error {
            ReleaseError::Fetch { url, error } => Self::Fetch { url, error },
            ReleaseError::Read { url, error } => Self::Read { url, error },
            ReleaseError::SumsNotText(error) => Self::SumsNotText { detail: chain(&error) },
            ReleaseError::Unlisted { asset } => Self::Unlisted { asset },
            ReleaseError::Mismatch { asset, actual, listed } => {
                Self::Mismatch { asset, actual, listed }
            }
            ReleaseError::CreateDir { dir, error } => {
                Self::CreateDir { dir: dir.display().to_string(), detail: chain(&error) }
            }
            ReleaseError::Write { path, error } => {
                Self::Write { path: path.display().to_string(), detail: chain(&error) }
            }
        }
    }
}

/// Why a release's image could not be had.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum FetchError {
    /// The release asset could not be fetched, checked or cached.
    #[error("the release asset: {error:?}")]
    Release { error: ReleaseFailure },
    /// The asset matched its release's sums and is no image the app takes.
    #[error("the release's image: {error}")]
    Image { error: ImageError },
    /// No node firmware is built for `board`.
    #[error("no node image for the board")]
    NoImage { board: Board },
    /// The fetch's work panicked.
    #[error("the fetch panicked")]
    Panicked,
}

/// A release's node image for `board`: fetched with the release's `SHA256SUMS`, checked against
/// it, kept under `cache_dir`, and checked as an image. A copy kept there that still matches is
/// used without a fetch.
///
/// # Errors
/// No node image for `board`, or the step that failed.
#[uniffi::export]
pub async fn fetch_node_image(
    tag: String,
    board: Board,
    cache_dir: String,
) -> Result<Arc<NodeImage>, FetchError> {
    fetch_node(Releases::live(), tag, board, cache_dir).await
}

/// A release's bridge image, as [`fetch_node_image`] has a node's.
///
/// # Errors
/// The step that failed.
#[uniffi::export]
pub async fn fetch_bridge_image(
    tag: String,
    cache_dir: String,
) -> Result<Arc<BridgeImage>, FetchError> {
    fetch_bridge(Releases::live(), tag, cache_dir).await
}

/// [`fetch_node_image`] from the releases at `base`, for a test's local listener.
///
/// # Errors
/// As [`fetch_node_image`].
#[cfg(feature = "testing")]
#[uniffi::export]
pub async fn fetch_node_image_from(
    base: String,
    tag: String,
    board: Board,
    cache_dir: String,
) -> Result<Arc<NodeImage>, FetchError> {
    fetch_node(Releases { base }, tag, board, cache_dir).await
}

/// [`fetch_bridge_image`] from the releases at `base`, for a test's local listener.
///
/// # Errors
/// As [`fetch_bridge_image`].
#[cfg(feature = "testing")]
#[uniffi::export]
pub async fn fetch_bridge_image_from(
    base: String,
    tag: String,
    cache_dir: String,
) -> Result<Arc<BridgeImage>, FetchError> {
    fetch_bridge(Releases { base }, tag, cache_dir).await
}

async fn fetch_node(
    releases: Releases,
    tag: String,
    board: Board,
    cache_dir: String,
) -> Result<Arc<NodeImage>, FetchError> {
    let asset = release::node_asset(board).filter(|_| has_node_image(board));
    let Some(asset) = asset else { return Err(FetchError::NoImage { board }) };
    let image = fetched(releases, tag, asset, cache_dir, app_image::NodeImage::parse).await?;
    Ok(Arc::new(NodeImage { image }))
}

async fn fetch_bridge(
    releases: Releases,
    tag: String,
    cache_dir: String,
) -> Result<Arc<BridgeImage>, FetchError> {
    let parse = app_image::BridgeImage::parse;
    let image = fetched(releases, tag, BRIDGE_APP_ASSET, cache_dir, parse).await?;
    Ok(Arc::new(BridgeImage { image }))
}

/// Release `tag`'s `asset`, fetched or cached and checked against its sums, then `parse`d, on the
/// blocking pool.
async fn fetched<T: Send + 'static>(
    releases: Releases,
    tag: String,
    asset: &'static str,
    cache_dir: String,
    parse: fn(Vec<u8>) -> Result<T, AppImageError>,
) -> Result<T, FetchError> {
    let fetching = runtime().spawn_blocking(move || {
        let cached = release::fetch(&releases, &tag, asset, Path::new(&cache_dir))
            .map_err(|error| FetchError::Release { error: error.into() })?;
        parse(cached.into_bytes()).map_err(|error| FetchError::Image { error: error.into() })
    });
    fetching.await.unwrap_or(Err(FetchError::Panicked))
}

/// Stops an update under way.
#[derive(Debug, uniffi::Object)]
pub struct Stopper {
    stopped: watch::Sender<bool>,
}

#[uniffi::export]
impl Stopper {
    /// A stopper not yet stopped.
    #[uniffi::constructor]
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self { stopped: watch::Sender::new(false) })
    }

    /// Stop the update it was given to, now or as soon as it starts.
    pub fn stop(&self) {
        self.stopped.send_replace(true);
    }
}

/// Resolves once `stopper` is stopped, or never without one.
async fn stopped(stopper: Option<Arc<Stopper>>) {
    let Some(stopper) = stopper else { return std::future::pending().await };
    let mut stopped = stopper.stopped.subscribe();
    // The sender lives in `stopper`, held here, so the wait ends only on a stop.
    if stopped.wait_for(|stopped| *stopped).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Why a node update, or the listen before it, could not run.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum UpdateError {
    /// A session or another call holds the link.
    #[error("the link is in use")]
    InUse,
    /// The link is closed, or its transport ended.
    #[error("the link is closed")]
    Closed,
    /// The Bluetooth link was opened with no bridge named, so the bridge it reaches may be one a
    /// scan heard. An app updates over a link it opened with `bridge` given; for the remembered
    /// bridge it passes `Settings.saved().bridge_mac`.
    #[error("no bridge was named when the link was opened")]
    BridgeNotNamed,
}

/// The link, lent for an update: refused unless it takes updates and is idle.
async fn lend(
    inner: &Inner,
) -> Result<(wartui_bridge::LinkHandle, Option<wartui_bridge::BridgeInfo>), UpdateError> {
    if !inner.takes_updates() {
        return Err(UpdateError::BridgeNotNamed);
    }
    inner.lend().await.map_err(|error| match error {
        LinkError::InUse => UpdateError::InUse,
        _ => UpdateError::Closed,
    })
}

/// What a node's heartbeat said it is.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NodeCapabilities {
    /// The firmware it runs, `0.7.0`.
    pub version: String,
    /// The board it was built for.
    pub board: Board,
    /// The radio reaches 5 GHz.
    pub five_ghz: bool,
    /// It runs an image it has not yet confirmed.
    pub trial: bool,
}

impl From<Capabilities> for NodeCapabilities {
    fn from(caps: Capabilities) -> Self {
        let Capabilities { version, board, five_ghz, trial } = caps;
        Self { version: version.to_string(), board, five_ghz, trial }
    }
}

/// A node an update may go to.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NodeTarget {
    /// Its whole address.
    pub node: String,
    /// What its latest heartbeat said it is.
    pub capabilities: NodeCapabilities,
    /// The slot it was heard in, labeled on the board the bridge announced, or `None` when
    /// another node's heartbeat has taken that slot since.
    pub slot: Option<SlotShown>,
}

/// `slot` labeled on `board`.
fn shown(slot: Slot, board: NodeBoard) -> SlotShown {
    SlotShown { index: slot.as_u8(), label: board.label(slot).to_string() }
}

impl NodeTarget {
    fn of(target: Target, board: NodeBoard) -> Self {
        let Target { node, caps, slot } = target;
        Self {
            node: mac::full(&node).to_string(),
            capabilities: caps.into(),
            slot: slot.map(|slot| shown(slot, board)),
        }
    }
}

/// Why no node was chosen, or no plan made.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum SelectFailed {
    /// No node heartbeated.
    #[error("no node heard")]
    NoneHeard,
    /// No node heartbeated from `slot`, which was named.
    #[error("no node heard in the slot named")]
    NoneInSlot { slot: SlotShown },
    /// `slot` names no node-bus slot.
    #[error("slot {slot} names no slot")]
    NoSuchSlot { slot: u8 },
    /// Two images were given for `board`.
    #[error("two images for one board")]
    TwoImages { board: Board },
}

/// What the run does with one node.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum NodeAction {
    /// Send it the image.
    Update,
    /// On trial on the image's version already: confirm it rather than send it again.
    Confirm,
    /// Already on the image's version.
    Same { version: String },
    /// On a newer version than the image.
    Older,
    /// No image was given for its board.
    NoImage { board: Board },
}

impl From<Action> for NodeAction {
    fn from(action: Action) -> Self {
        match action {
            Action::Update => Self::Update,
            Action::Confirm => Self::Confirm,
            Action::Same(version) => Self::Same { version: version.to_string() },
            Action::Older => Self::Older,
            Action::NoImage(board) => Self::NoImage { board },
        }
    }
}

/// One node of a plan.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PlanRow {
    pub target: NodeTarget,
    /// The version of the image for its board, if one was given.
    pub image: Option<String>,
    /// What happens to it.
    pub action: NodeAction,
}

/// The nodes a listen heard, and the board their slots are labeled on.
#[derive(Debug, uniffi::Object)]
pub struct NodesHeard {
    heard: Heard,
    board: NodeBoard,
}

#[uniffi::export]
impl NodesHeard {
    /// The nodes in `slots` (as a view or a target shows each; every node heard when empty), as
    /// update targets, in address order. A slot is named by its index, and its label is not
    /// read.
    ///
    /// # Errors
    /// No node heard, none in a slot named, or an index that names no slot.
    pub fn targets(&self, slots: Vec<SlotShown>) -> Result<Vec<NodeTarget>, SelectFailed> {
        let targets = self.select(&slots)?;
        Ok(targets.into_iter().map(|target| NodeTarget::of(target, self.board)).collect())
    }

    /// The boards those targets need an image for, each once. A board no node image is built
    /// for is left out, and the plan refuses its nodes.
    ///
    /// # Errors
    /// As [`Self::targets`].
    pub fn boards(&self, slots: Vec<SlotShown>) -> Result<Vec<Board>, SelectFailed> {
        Ok(nodes::boards(&self.select(&slots)?))
    }

    /// What would happen to each target with these images: the plan a confirm screen shows, and
    /// the plan that runs. `force` also updates a node already on the image's version, or on a
    /// newer one.
    ///
    /// # Errors
    /// As [`Self::targets`], and [`SelectFailed::TwoImages`] for two images of one board.
    pub fn plan(
        &self,
        slots: Vec<SlotShown>,
        images: Vec<Arc<NodeImage>>,
        force: bool,
    ) -> Result<Arc<NodePlan>, SelectFailed> {
        let targets = self.select(&slots)?;
        let mut by_board = HashMap::new();
        for image in &images {
            let board = image.image.board();
            // A clone of the image's handle on its bytes.
            if by_board.insert(board, image.image.clone()).is_some() {
                return Err(SelectFailed::TwoImages { board });
            }
        }
        let plan = Plan::of(targets, by_board, force);
        let rows = plan.rows().iter().map(|row| plan_row(row, self.board)).collect();
        Ok(Arc::new(NodePlan { plan, rows }))
    }
}

impl NodesHeard {
    fn select(&self, slots: &[SlotShown]) -> Result<Vec<Target>, SelectFailed> {
        let named = slots
            .iter()
            .map(|slot| slot.index)
            .map(|slot| Slot::from_u8(slot).ok_or(SelectFailed::NoSuchSlot { slot }))
            .collect::<Result<Vec<_>, _>>()?;
        nodes::select(&self.heard, &named).map_err(|error| match error {
            SelectError::NoneHeard => SelectFailed::NoneHeard,
            SelectError::NoneInSlot(slot) => {
                SelectFailed::NoneInSlot { slot: shown(slot, self.board) }
            }
        })
    }
}

fn plan_row(row: &Row, board: NodeBoard) -> PlanRow {
    let Row { target, image, action } = *row;
    PlanRow {
        target: NodeTarget::of(target, board),
        image: image.map(|version| version.to_string()),
        action: action.into(),
    }
}

/// What an update does with each node, and the images it sends. Opaque, so the plan shown is the
/// plan run.
#[derive(Debug, uniffi::Object)]
pub struct NodePlan {
    plan: Plan,
    rows: Vec<PlanRow>,
}

#[uniffi::export]
impl NodePlan {
    /// Each node, in address order.
    #[must_use]
    pub fn rows(&self) -> Vec<PlanRow> {
        self.rows.clone()
    }

    /// How many nodes it would update or confirm.
    #[must_use]
    pub fn chosen(&self) -> u32 {
        u32::try_from(self.plan.chosen()).unwrap_or(u32::MAX)
    }
}

/// Why a sender of an image gave up: the bridge's node image cache, or the bridge itself.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum SenderFailure {
    /// The receiver reported this state: a hash mismatch, a flash error, another board or an
    /// image too large for its slot.
    Refused { state: UpdateState },
    /// The receiver verified the image, then could not switch to it.
    SwitchFailed { state: UpdateState },
    /// Every send of one op went unanswered.
    Silent,
    /// The receiver kept reporting no session.
    LostSession,
}

impl From<AppSenderFailure> for SenderFailure {
    fn from(failure: AppSenderFailure) -> Self {
        match failure {
            AppSenderFailure::Refused(state) => Self::Refused { state },
            AppSenderFailure::SwitchFailed(state) => Self::SwitchFailed { state },
            AppSenderFailure::Silent => Self::Silent,
            AppSenderFailure::LostSession => Self::LostSession,
        }
    }
}

/// Why one node's update failed.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum NodeFailure {
    /// No status answered the `begin` in time.
    NoAnswer,
    /// The node refused the `begin`: it has no update slot or `trial` sector, or its flash
    /// failed.
    Refused,
    /// The node is a different board from the image's.
    WrongBoard,
    /// The image is larger than the node's update slot.
    TooLarge,
    /// Writing the node's flash failed.
    FlashError,
    /// Every byte arrived and the hash did not match.
    HashMismatch,
    /// The update started over too many times and lost the node again.
    NotAnswering,
    /// The image verified and the node could not switch slots to it.
    SwitchFailed,
    /// No heartbeat from the new image in time.
    NoReboot,
    /// The old image, `version`, is the one heartbeating.
    StillOld { version: String },
    /// The new image booted, and then the old one, `version`, came back.
    RolledBack { version: String },
    /// The new image ran unconfirmed for too long.
    Unconfirmed,
    /// The node answered with a state this build has no name for.
    UnknownState { state: u8 },
    /// The update was stopped.
    Canceled,
    /// The link to the bridge closed.
    LinkClosed,
    /// The bridge's cache lost the image it had checked.
    CacheLost,
}

impl From<Failure> for NodeFailure {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::NoAnswer => Self::NoAnswer,
            Failure::Refused => Self::Refused,
            Failure::WrongBoard => Self::WrongBoard,
            Failure::TooLarge => Self::TooLarge,
            Failure::FlashError => Self::FlashError,
            Failure::HashMismatch => Self::HashMismatch,
            Failure::NotAnswering => Self::NotAnswering,
            Failure::SwitchFailed => Self::SwitchFailed,
            Failure::NoReboot => Self::NoReboot,
            Failure::StillOld(version) => Self::StillOld { version: version.to_string() },
            Failure::RolledBack(version) => Self::RolledBack { version: version.to_string() },
            Failure::Unconfirmed => Self::Unconfirmed,
            Failure::UnknownState(state) => Self::UnknownState { state },
            Failure::Canceled => Self::Canceled,
            Failure::LinkClosed => Self::LinkClosed,
            Failure::CacheLost => Self::CacheLost,
        }
    }
}

/// How one node's update ended.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum NodeOutcome {
    /// The node runs the new image, `version`, confirmed.
    Done { version: String },
    /// It does not.
    Failed { failure: NodeFailure },
}

impl From<Outcome> for NodeOutcome {
    fn from(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Done(version) => Self::Done { version: version.to_string() },
            Outcome::Failed(failure) => Self::Failed { failure: failure.into() },
        }
    }
}

/// A node, and how its update ended.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NodeEnded {
    /// Its whole address.
    pub node: String,
    pub outcome: NodeOutcome,
}

fn ended(outcomes: Vec<(Mac, Outcome)>) -> Vec<NodeEnded> {
    outcomes
        .into_iter()
        .map(|(node, outcome)| NodeEnded {
            node: mac::full(&node).to_string(),
            outcome: outcome.into(),
        })
        .collect()
}

/// Where one node's update stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct SessionProgress {
    pub phase: Phase,
    /// Bytes the node has reported holding.
    pub acked: u32,
    /// Bytes in the image.
    pub total: u32,
}

impl From<ota::Progress> for SessionProgress {
    fn from(progress: ota::Progress) -> Self {
        let ota::Progress { phase, acked, total } = progress;
        Self { phase, acked, total }
    }
}

/// What a node update reports as it goes.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum NodeUpdateProgress {
    /// The bridge did not report its scan stopped in time; the update goes on.
    ScanNotStopped,
    /// A fleet starts: its nodes' addresses, and the image's size when it sends one.
    Fleet { board: Option<Board>, nodes: Vec<String>, image_bytes: Option<u64> },
    /// The bridge checked the image in its cache, `took_ms` after the fleet started.
    Cached { took_ms: u32 },
    /// Where a node's update stands.
    Node { node: String, progress: SessionProgress },
    /// The fleet ended: each node that has an outcome, with it.
    FleetEnded { outcomes: Vec<NodeEnded> },
}

/// `duration` in milliseconds, saturating.
fn millis(duration: Duration) -> u32 {
    u32::try_from(duration.as_millis()).unwrap_or(u32::MAX)
}

impl From<nodes::Progress> for NodeUpdateProgress {
    fn from(progress: nodes::Progress) -> Self {
        match progress {
            nodes::Progress::ScanNotStopped => Self::ScanNotStopped,
            nodes::Progress::Fleet { board, nodes, image_bytes } => Self::Fleet {
                board,
                nodes: nodes.iter().map(|node| mac::full(node).to_string()).collect(),
                image_bytes: image_bytes.map(wide),
            },
            nodes::Progress::Cached { took } => Self::Cached { took_ms: millis(took) },
            nodes::Progress::Node { node, progress } => {
                Self::Node { node: mac::full(&node).to_string(), progress: progress.into() }
            }
            nodes::Progress::FleetEnded { outcomes } => {
                Self::FleetEnded { outcomes: ended(outcomes) }
            }
        }
    }
}

/// Why an update ended before every chosen node had its turn.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum CutShort {
    /// The stop came before any node was told anything.
    BeforeStart,
    /// The bridge refused or lost the upload into its cache.
    Upload { failure: SenderFailure },
    /// The bridge lost the image after checking it.
    CacheLost,
}

impl From<AppCutShort> for CutShort {
    fn from(cut: AppCutShort) -> Self {
        match cut {
            AppCutShort::BeforeStart => Self::BeforeStart,
            AppCutShort::Upload(failure) => Self::Upload { failure: failure.into() },
            AppCutShort::CacheLost => Self::CacheLost,
        }
    }
}

/// How an update that did not update every chosen node failed.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum NodesFailed {
    /// The bridge did not take the node image, after `done` nodes were updated.
    Upload { failure: SenderFailure, done: u32 },
    /// The bridge lost the node image.
    CacheLost,
    /// The update was stopped.
    Canceled,
    /// `failed` of `total` nodes did not update.
    Nodes { failed: u32, total: u32 },
}

/// `count` as a `u32`, saturating.
fn count(count: usize) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}

impl From<AppFailed> for NodesFailed {
    fn from(failed: AppFailed) -> Self {
        match failed {
            AppFailed::Upload { failure, done } => {
                Self::Upload { failure: failure.into(), done: count(done) }
            }
            AppFailed::CacheLost => Self::CacheLost,
            AppFailed::Canceled => Self::Canceled,
            AppFailed::Nodes { failed, total } => {
                Self::Nodes { failed: count(failed), total: count(total) }
            }
        }
    }
}

/// How a node update ended.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NodesUpdated {
    /// Every node's that has one, in the order the fleets ran.
    pub outcomes: Vec<NodeEnded>,
    /// Why the update ended before every chosen node had its turn, if it did.
    pub cut_short: Option<CutShort>,
    /// How it failed, or `None` when every node with an outcome updated.
    pub failed: Option<NodesFailed>,
}

impl From<Ended> for NodesUpdated {
    fn from(ended: Ended) -> Self {
        let failed = ended.verdict().err().map(NodesFailed::from);
        let Ended { outcomes, cut_short } = ended;
        Self { outcomes: self::ended(outcomes), cut_short: cut_short.map(CutShort::from), failed }
    }
}

/// What a node update says as it goes.
#[uniffi::export(with_foreign)]
pub trait NodeUpdateListener: Send + Sync {
    /// One step, on any thread. A listener that fails is ignored.
    fn progress(&self, progress: NodeUpdateProgress) -> Result<(), SinkError>;
}

/// How a bridge update that did not fail ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum BridgeUpdated {
    /// The bridge already runs the image; nothing was sent.
    AlreadyCurrent,
    /// The bridge runs the image, confirmed.
    Installed,
}

impl From<Updated> for BridgeUpdated {
    fn from(updated: Updated) -> Self {
        match updated {
            Updated::AlreadyCurrent => Self::AlreadyCurrent,
            Updated::Installed => Self::Installed,
        }
    }
}

/// Why a command could not be queued on the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum QueueFailure {
    /// The transport has shut down.
    Closed,
    /// The queue is full.
    Full,
}

/// Which link an update went over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LinkKind {
    Usb,
    Bluetooth,
}

/// Why a bridge update failed, or did not start.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum BridgeUpdateFailed {
    /// The link dropped while the image was sent, with the transport's reason.
    #[error("the link dropped before the update finished: {reason}")]
    Dropped { reason: String },
    /// The link is closed, or its transport ended.
    #[error("the link closed")]
    Closed,
    /// While sending, the bridge restarted into another image than the send began against.
    #[error("the bridge restarted into another image while sending")]
    RestartedIntoOther,
    /// The link stopped writing the image at `percent`.
    #[error("the link stopped taking the image at {percent}%")]
    PortStalled { percent: u8 },
    /// The sender gave up; `image_known`: the bridge's announcement named an image before the
    /// update, which says its firmware takes updates.
    #[error("the sender gave up")]
    Sender { failure: SenderFailure, image_known: bool },
    /// An op could not be queued.
    #[error("an update op could not be queued")]
    Queue { failure: QueueFailure },
    /// No new life answered within `secs` of the install, over `over`.
    #[error("the bridge did not come back within {secs} s")]
    NoComeBack { over: LinkKind, secs: u64 },
    /// Over Bluetooth, a `Ready` from the life before the install: the install never reached it.
    #[error("the bridge never took the install")]
    NeverTookInstall,
    /// The bridge came back on the image it ran before.
    #[error("rolled back")]
    RolledBack,
    /// The bridge came back on neither image, or one it could not name.
    #[error("came back running another image")]
    OtherImage,
    /// The bridge runs the new image, and says confirming it failed, in its words.
    #[error("installed, not confirmed: {why}")]
    NotConfirmed { why: String },
    /// The bridge runs the new image, and no confirm was seen: within `secs` of its `Ready`, or
    /// before the wait over Bluetooth ended (`None`).
    #[error("installed, no confirm seen")]
    NoConfirm { secs: Option<u64> },
    /// Over Bluetooth, the wait ended with no new life judged.
    #[error("the bridge did not come back")]
    NotBack,
    /// A session or another call holds the link.
    #[error("the link is in use")]
    InUse,
    /// The link holds no announcement from the bridge, which names the image it runs: wait for
    /// one with `ready` first.
    #[error("no bridge has announced itself")]
    NotAnnounced,
    /// As [`UpdateError::BridgeNotNamed`].
    #[error("no bridge was named when the link was opened")]
    BridgeNotNamed,
    /// The stopper stopped it. The bridge keeps the image it runs.
    #[error("stopped")]
    Stopped,
}

impl From<Failed> for BridgeUpdateFailed {
    fn from(failed: Failed) -> Self {
        match failed {
            Failed::Dropped { reason } => Self::Dropped { reason },
            Failed::Closed => Self::Closed,
            Failed::RestartedIntoOther => Self::RestartedIntoOther,
            Failed::PortStalled { percent } => Self::PortStalled { percent },
            Failed::Sender { failure, image_known } => {
                Self::Sender { failure: failure.into(), image_known }
            }
            Failed::Queue(error) => Self::Queue {
                failure: match error {
                    SendError::Closed => QueueFailure::Closed,
                    SendError::Full => QueueFailure::Full,
                },
            },
            Failed::NoComeBack { over, secs } => {
                let over = match over {
                    Over::Usb => LinkKind::Usb,
                    Over::Bluetooth => LinkKind::Bluetooth,
                };
                Self::NoComeBack { over, secs }
            }
            Failed::NeverTookInstall => Self::NeverTookInstall,
            Failed::RolledBack => Self::RolledBack,
            Failed::OtherImage => Self::OtherImage,
            Failed::NotConfirmed { why } => Self::NotConfirmed { why },
            Failed::NoConfirm { secs } => Self::NoConfirm { secs },
            Failed::NotBack => Self::NotBack,
        }
    }
}

/// What a bridge update reports as it goes, in the order it happens.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum BridgeUpdateProgress {
    /// Over Bluetooth, the bridge did not report its scan stopped; the update goes on.
    ScanNotStopped,
    /// The share of the image the bridge has reported holding, said on every turn of the send.
    Sending { percent: u8 },
    /// `Install` is sent; the bridge reboots.
    Installing,
    /// Over Bluetooth, the link dropped as the bridge restarts. Said once.
    LinkDropped,
    /// The new image is running and confirmed, known this way.
    Confirmed { proof: Proof },
    /// The radio restarted on its bundled image.
    RadioRestarted,
    /// The radio's update is still running when the wait for it ends.
    RadioStillUpdating,
    /// A line the bridge sent meanwhile.
    Said { line: Line },
}

impl From<bridge::Progress> for BridgeUpdateProgress {
    fn from(progress: bridge::Progress) -> Self {
        match progress {
            bridge::Progress::ScanNotStopped => Self::ScanNotStopped,
            bridge::Progress::Sending { percent } => Self::Sending { percent },
            bridge::Progress::Installing => Self::Installing,
            bridge::Progress::LinkDropped => Self::LinkDropped,
            bridge::Progress::Confirmed(proof) => Self::Confirmed { proof },
            bridge::Progress::RadioRestarted => Self::RadioRestarted,
            bridge::Progress::RadioStillUpdating => Self::RadioStillUpdating,
            bridge::Progress::Line(line) => Self::Said { line },
        }
    }
}

/// What a bridge update says as it goes.
#[uniffi::export(with_foreign)]
pub trait BridgeUpdateListener: Send + Sync {
    /// One step, on any thread. A listener that fails is ignored.
    fn progress(&self, progress: BridgeUpdateProgress) -> Result<(), SinkError>;
}

#[uniffi::export]
impl Link {
    /// Listen `period_ms` for the fleet's heartbeats. The link is in use while this runs. Six
    /// seconds hears an assigned node, which beats every five. A link that holds no announcement
    /// waits up to `period_ms` for one first, since the slots are labeled on the board it names.
    ///
    /// # Errors
    /// [`UpdateError::BridgeNotNamed`] for a Bluetooth link opened unnamed,
    /// [`UpdateError::InUse`] or [`UpdateError::Closed`] for a link not idle, and
    /// [`UpdateError::Closed`] when the transport ends while listening.
    pub async fn hear_nodes(&self, period_ms: u32) -> Result<Arc<NodesHeard>, UpdateError> {
        let inner = self.shared();
        let period = Duration::from_millis(u64::from(period_ms));
        // On the runtime, so a phone that stops awaiting still has the link given back.
        match spawn(hear(Arc::clone(&inner), period)).await {
            Ok(heard) => heard,
            // The link was in the work that panicked, and went with it.
            Err(_) => {
                inner.lose();
                Err(UpdateError::Closed)
            }
        }
    }

    /// Run `plan`: stop the scan, then update each node it chooses, every one at once, reporting
    /// through `listener`. `stopper` cancels it, and each node under way is told. A plan that
    /// chooses no node sends nothing.
    ///
    /// # Errors
    /// As [`Self::hear_nodes`], except a transport that ends, which fails each node instead.
    pub async fn update_nodes(
        &self,
        plan: Arc<NodePlan>,
        stopper: Option<Arc<Stopper>>,
        listener: Option<Arc<dyn NodeUpdateListener>>,
    ) -> Result<NodesUpdated, UpdateError> {
        let inner = self.shared();
        match spawn(update_nodes(Arc::clone(&inner), plan, stopper, listener)).await {
            Ok(updated) => updated,
            Err(_) => {
                inner.lose();
                Err(UpdateError::Closed)
            }
        }
    }

    /// Send `image` to the bridge over this link, wait out its reboot, and read the outcome.
    /// Needs the bridge's announcement (`ready`), which names the image it runs, and clears it
    /// once done, whatever the outcome. `stopper` abandons it; the bridge keeps the image it runs.
    ///
    /// A phone has no USB to fall back on, so a bridge update that fails over Bluetooth is
    /// recovered from a computer; the app says so before it starts. It can run several minutes
    /// and reads the outcome across the bridge's reconnects, so the app runs it in a background
    /// task that is not suspended.
    ///
    /// # Errors
    /// How the update failed, or why it did not start: [`BridgeUpdateFailed::BridgeNotNamed`],
    /// [`BridgeUpdateFailed::InUse`], [`BridgeUpdateFailed::Closed`] or
    /// [`BridgeUpdateFailed::NotAnnounced`].
    pub async fn update_bridge(
        &self,
        image: Arc<BridgeImage>,
        stopper: Option<Arc<Stopper>>,
        listener: Option<Arc<dyn BridgeUpdateListener>>,
    ) -> Result<BridgeUpdated, BridgeUpdateFailed> {
        let inner = self.shared();
        let working = Arc::clone(&inner);
        // On the runtime, so a phone that stops awaiting still has the link given back.
        match spawn(update_bridge(working, image, stopper, listener)).await {
            Ok(updated) => updated,
            Err(_) => {
                inner.lose();
                Err(BridgeUpdateFailed::Closed)
            }
        }
    }
}

/// [`Link::hear_nodes`]'s work, on the library's runtime.
async fn hear(inner: Arc<Inner>, period: Duration) -> Result<Arc<NodesHeard>, UpdateError> {
    let (mut handle, mut announced) = lend(&inner).await?;
    if announced.is_none() {
        // A listen reads past an announcement, which the link would then never hold, so one not
        // yet held is waited for first, up to the listen's own length.
        let over = if inner.is_simulated() { Over::Usb } else { Over::Bluetooth };
        let waited = ops::ready(&mut handle, over, period, ops::REPLY_TIMEOUT, &mut |_| {}).await;
        announced = waited.ready.ok().flatten();
    }
    let heard = nodes::listen(&mut handle, period).await;
    // Labeled as a capture's view labels them: no carrier before an announcement.
    let board = announced.as_ref().map_or(NodeBoard::NoCarrier, |bridge| bridge.node_board);
    inner.give_back(handle, announced);
    match heard {
        Ok(heard) => Ok(Arc::new(NodesHeard { heard, board })),
        Err(LinkClosed) => Err(UpdateError::Closed),
    }
}

/// [`Link::update_nodes`]'s work, on the library's runtime.
async fn update_nodes(
    inner: Arc<Inner>,
    plan: Arc<NodePlan>,
    stopper: Option<Arc<Stopper>>,
    listener: Option<Arc<dyn NodeUpdateListener>>,
) -> Result<NodesUpdated, UpdateError> {
    let (mut handle, announced) = lend(&inner).await?;
    let say = |progress: nodes::Progress| {
        if let Some(listener) = &listener {
            // A listener that fails is the phone's to notice; the update goes on.
            let _ = listener.progress(progress.into());
        }
    };
    let ended = nodes::run(&mut handle, &plan.plan, stopped(stopper), say).await;
    inner.give_back(handle, announced);
    Ok(ended.into())
}

/// [`Link::update_bridge`]'s work, on the library's runtime.
async fn update_bridge(
    inner: Arc<Inner>,
    image: Arc<BridgeImage>,
    stopper: Option<Arc<Stopper>>,
    listener: Option<Arc<dyn BridgeUpdateListener>>,
) -> Result<BridgeUpdated, BridgeUpdateFailed> {
    let (mut handle, announced) = lend(&inner).await.map_err(|error| match error {
        UpdateError::InUse => BridgeUpdateFailed::InUse,
        UpdateError::Closed => BridgeUpdateFailed::Closed,
        UpdateError::BridgeNotNamed => BridgeUpdateFailed::BridgeNotNamed,
    })?;
    let Some(running) = announced.map(|bridge| bridge.image) else {
        inner.give_back(handle, None);
        return Err(BridgeUpdateFailed::NotAnnounced);
    };
    // The simulator stands in for a bridge on USB, as a capture over it does.
    let over = if inner.is_simulated() { Over::Usb } else { Over::Bluetooth };
    let say = |progress: bridge::Progress| {
        if let Some(listener) = &listener {
            // A listener that fails is the phone's to notice; the update goes on.
            let _ = listener.progress(progress.into());
        }
    };
    // Dropping the update's future is how it stops; the bridge drops its session with it.
    let updated = tokio::select! {
        updated = bridge::update(&mut handle, over, running, &image.image, say) => {
            updated.map(BridgeUpdated::from).map_err(BridgeUpdateFailed::from)
        }
        () = stopped(stopper) => Err(BridgeUpdateFailed::Stopped),
    };
    // The bridge may run another image now, so the next `ready` or capture reads it afresh.
    inner.give_back(handle, None);
    updated
}
