//! The saved settings, and the one path that writes them.
//!
//! **Every write goes through one function** (`State::save`). It overlays the bridge the
//! transport holds in the shared [`BridgeMemory`], so a save never writes back a bridge the
//! transport has since replaced or forgotten. It writes `[bridge] link` only once a bridge has
//! announced itself over the run's link, since a link that never connected is no link to
//! remember. It makes `remember` follow the live switch, whatever an earlier failed save left in
//! the copy, so `remember` never contradicts `mac`. On success the written copy becomes the saved
//! one. [`Settings`] is the store's only writer within a run, so the transport's thread never
//! races it on a whole-store save; the store is tiny, so the write happens under the lock.
//!
//! **[`apply`], behind `Controls::apply`, puts a front end's rows in force, then saves them.** It reserves a command slot
//! for every row that moved before it sends any, so a full queue sends nothing and saves nothing:
//! the saved copy would claim settings the engine is not running. The bridge's rows go only when
//! they moved, and never reach the store: the bridge stores every pool, planner, duty,
//! wardrive esp and log setting it is sent, and an unmoved bluetooth row would restate the scan. A
//! row left where the store put it keeps the store's spelling, a hand-written `true` included;
//! only a moved row is rewritten. [`apply_excluded`] applies and saves only the excluded list, and
//! [`Settings::save_wait_for_fix`] saves only `[gps] wait-for-fix`.
//!
//! **[`Settings::follow`], behind `Controls::follow`, saves what changed without the operator.** The transport remembers a
//! bridge it connects to and forgets one that goes quiet, so a snapshot whose bridge no longer
//! matches the saved copy is saved, silently. A failed save is tried again only once the bridge
//! changes, rather than every frame. The run's link is saved once, when the first bridge announces
//! itself over it.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::mpsc;
use wartui_bridge::remember::BridgeMemory;
use wartui_core::engine::{Command, Snapshot};
use wartui_core::ubx;
use wartui_proto::fan::FanDuty;
use wartui_proto::mac::Mac;
use wartui_proto::plan::{ChannelPool, PlanSettings};

use super::{
    ApiKeys, Bridge, Config, Gps, Link, SerializeError, SettingsStore, excluded_ssid_bytes,
    serialize,
};

/// Why a save wrote nothing.
#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    /// There is no store: a desktop with no default location and no file named.
    #[error("no settings store")]
    Nowhere,
    /// The settings break a rule, so writing them would leave text a load refuses.
    #[error(transparent)]
    Invalid(SerializeError),
    /// The store's own failure.
    #[error(transparent)]
    Store(std::io::Error),
}

/// The engine takes no command: its queue is full, or it has stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy;

/// The settings modal's rows as plain data. An engine-bound row is `Some` only when it moved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsEdit {
    pub pool: Option<ChannelPool>,
    /// The planner and its sparse node count, which one command carries, so either moving sends
    /// both.
    pub planner: Option<PlanSettings>,
    pub fan: Option<FanDuty>,
    pub bluetooth: Option<bool>,
    pub wardrive_esp: Option<bool>,
    pub sd_log: Option<bool>,
    /// The bridge receiver's rate. Unmoved means the rate in force, `snapshot.gps_rate_hz`, which
    /// the store keeps as it had it.
    pub gps_rate: Option<u8>,
    /// Whether `run` remembers the bridge it connects to.
    pub remember_bridge: bool,
    /// Whether `run` holds the scan for a GPS fix, from the next launch.
    pub wait_for_fix: bool,
    /// The keys as typed, which are trimmed before they are saved.
    pub api_keys: ApiKeys,
    /// The list in force, which is saved and not sent.
    pub excluded_ssids: Vec<String>,
}

/// What [`Controls::apply`](crate::session::Controls::apply) did beyond sending the edit's moved
/// rows.
#[derive(Debug)]
pub struct Applied {
    /// The remember switch, `Some` when it changed.
    pub remember: Option<bool>,
    /// Whether the wait-for-fix row moved. It lives only in the store, so it changes nothing
    /// unless the save lands.
    pub wait_for_fix_moved: bool,
    /// The store's place, or why nothing was saved. The rows are in force either way.
    pub saved: Result<String, SaveError>,
}

/// The saved settings and the one path that writes them. Cheap to clone; clones share one state.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    state: Arc<Mutex<State>>,
}

#[derive(Debug, Default)]
struct State {
    /// Where the settings are kept, or `None` with nowhere to keep them.
    store: Option<Arc<dyn SettingsStore>>,
    /// The settings as last loaded or saved. A remembered bridge rewrites only the bridge from
    /// this copy.
    saved: Config,
    /// The bridge `run` remembers, and whether it remembers one. Shared with the transport, so the
    /// remember switch takes effect on the next reconnect and the bridge the transport connects to
    /// reaches the store.
    memory: BridgeMemory,
    /// Whether the link is the simulator, whose bridge is not a board to remember.
    simulated: bool,
    /// The link this run reaches its bridge over, saved as `[bridge] link` once it connects;
    /// `None` under the simulator, which saves no link.
    link: Option<Link>,
    /// Whether a bridge has announced itself over [`Self::link`]. Until then a save keeps
    /// `[bridge] link` as the store had it.
    link_proved: bool,
    /// The bridge [`Settings::follow`] last tried to save, so a failed save waits for the next
    /// change rather than repeating every frame.
    bridge_tried: Option<(Option<Mac>, Option<String>, Option<String>)>,
    /// Whether [`Settings::follow`] has tried to save the link, which it does once.
    link_tried: bool,
}

impl Settings {
    /// Settings kept in `store`, last loaded as `saved`, with the bridge `memory` holds.
    #[must_use]
    pub fn new(store: Option<Arc<dyn SettingsStore>>, saved: Config, memory: BridgeMemory) -> Self {
        let state = State { store, saved, memory, ..State::default() };
        Self { state: Arc::new(Mutex::new(state)) }
    }

    /// The link this run reaches its bridge over. Unset under the simulator.
    #[must_use]
    pub fn over(self, link: Link) -> Self {
        self.lock().link = Some(link);
        self
    }

    /// The simulator's bridge, which is not a board to remember.
    #[must_use]
    pub fn simulated(self) -> Self {
        self.lock().simulated = true;
        self
    }

    /// The settings as last loaded or saved.
    #[must_use]
    pub fn saved(&self) -> Config {
        self.lock().saved.clone()
    }

    /// Whether `run` waits for a GPS fix at launch, as the settings were last loaded or saved.
    #[must_use]
    pub fn waits_for_fix(&self) -> bool {
        self.lock().saved.gps.waits_for_fix()
    }

    /// The link this run reaches its bridge over, or `None` under the simulator.
    #[must_use]
    pub fn link(&self) -> Option<Link> {
        self.lock().link
    }

    /// The bridge the transport remembers, and whether it remembers one.
    #[must_use]
    pub fn memory(&self) -> BridgeMemory {
        self.lock().memory.clone()
    }

    /// The store's place, or `None` with no store.
    #[must_use]
    pub fn place(&self) -> Option<String> {
        self.lock().store.as_ref().map(|store| store.place())
    }

    /// Save whether `run` waits for a GPS fix at launch, over the settings as last saved. Nothing
    /// else changes.
    ///
    /// # Errors
    /// Why nothing was saved.
    pub fn save_wait_for_fix(&self, wait: bool) -> Result<String, SaveError> {
        let mut state = self.lock();
        let mut written = state.saved.clone();
        written.gps.wait_for_fix = wait_for_fix_key(wait);
        state.save(written)
    }

    /// Save the bridge the transport remembered or forgot, and the link once a bridge has
    /// announced itself over it. Silent, since nothing the operator did changed either: a failure
    /// is logged at `debug`.
    pub(crate) fn follow(&self, snapshot: &Snapshot) {
        let mut state = self.lock();
        state.follow_bridge();
        state.follow_link(snapshot);
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Every write leaves the state whole, so a panic elsewhere leaves nothing half-done.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl State {
    /// Write `written` to the store, with the bridge [`Self::memory`] holds, and on success make
    /// it [`Self::saved`]. Returns the store's place.
    fn save(&mut self, mut written: Config) -> Result<String, SaveError> {
        let Some(store) = self.store.clone() else { return Err(SaveError::Nowhere) };
        // Nothing while remembering is off, so the keys drop out beside `remember = false`.
        written.bridge.mac = self.memory.recall();
        written.bridge.serial = self.memory.recall_serial();
        written.bridge.bluetooth_address = self.memory.recall_bluetooth();
        if let Some(link) = self.link.filter(|_| self.link_proved) {
            written.bridge.link = link_written(link, written.bridge.link);
        }
        // The live switch, whatever an earlier failed save left in `saved`, so `remember` never
        // contradicts `mac`. An absent key stays absent while on, since absent means on.
        written.bridge.remember = match (self.memory.is_enabled(), written.bridge.remember) {
            (true, None) => None,
            (on, _) => Some(on),
        };
        let text = serialize(&written).map_err(SaveError::Invalid)?;
        store.save(&text, written.api_keys.any_set()).map_err(SaveError::Store)?;
        self.saved = written;
        Ok(store.place())
    }

    /// Write every row of `edit` to the store, `rate` being the receiver's rate the row shows.
    ///
    /// Replaces the whole copy, hand edits included. The rows are already applied, so a failed
    /// save loses only the stored copy.
    fn save_edit(&mut self, edit: &SettingsEdit, rate: u8) -> Result<String, SaveError> {
        // A row left where the store put it keeps the store's spelling, a hand-written `true`
        // included; only a moved row is rewritten.
        let wait_for_fix = if edit.wait_for_fix == self.saved.gps.waits_for_fix() {
            self.saved.gps.wait_for_fix
        } else {
            wait_for_fix_key(edit.wait_for_fix)
        };
        let saved_rate = self.saved.gps.bridge_rate_hz;
        let bridge_rate_hz = if rate == saved_rate.unwrap_or(ubx::DEFAULT_RATE_HZ) {
            saved_rate
        } else {
            Some(rate)
        };
        let written = Config {
            excluded_ssids: edit.excluded_ssids.clone(),
            bridge: Bridge {
                // Not a row, so kept as the store last had it, unless the run saves its own.
                link: self.saved.bridge.link,
                remember: Some(edit.remember_bridge),
                ..Bridge::default()
            },
            gps: Gps { bridge_rate_hz, wait_for_fix },
            api_keys: ApiKeys {
                wdgwars: edit.api_keys.wdgwars.trim().to_owned(),
                wigle_name: edit.api_keys.wigle_name.trim().to_owned(),
                wigle_token: edit.api_keys.wigle_token.trim().to_owned(),
            },
        };
        self.save(written)
    }

    /// Save the bridge the transport remembered or forgot since the last save. A failure is tried
    /// again only once the bridge changes.
    fn follow_bridge(&mut self) {
        // Compared in place, since this runs every frame; cloned only once it has changed.
        let memory = &self.memory;
        let saved = &self.saved.bridge;
        if memory.matches(saved.mac, saved.serial.as_deref(), saved.bluetooth_address.as_deref()) {
            // In step with the store, so a bridge whose save failed is tried again when it comes
            // back.
            self.bridge_tried = None;
            return;
        }
        if let Some((mac, serial, bluetooth)) = &self.bridge_tried
            && memory.matches(*mac, serial.as_deref(), bluetooth.as_deref())
        {
            return;
        }
        self.bridge_tried =
            Some((memory.recall(), memory.recall_serial(), memory.recall_bluetooth()));
        if let Err(error) = self.save(self.saved.clone()) {
            tracing::debug!(%error, "could not save the bridge");
        }
    }

    /// Save the link this run reaches its bridge over, once the bridge has announced itself over
    /// it, so the next run takes the same link. Tried once.
    fn follow_link(&mut self, snapshot: &Snapshot) {
        let Some(link) = self.link else { return };
        if self.link_tried || snapshot.bridge.is_none() {
            return;
        }
        self.link_proved = true;
        let saved = self.saved.bridge.link;
        if link_written(link, saved) == saved {
            return;
        }
        self.link_tried = true;
        if let Err(error) = self.save(self.saved.clone()) {
            tracing::debug!(%error, "could not save the link");
        }
    }
}

/// `[gps] wait-for-fix` as written for `wait`: absent to wait, `false` to skip.
const fn wait_for_fix_key(wait: bool) -> Option<bool> {
    if wait { None } else { Some(false) }
}

/// What `[bridge] link` says once `link` is saved over `saved`: absent stays absent for USB,
/// since absent means USB.
fn link_written(link: Link, saved: Option<Link>) -> Option<Link> {
    match (link, saved) {
        (Link::Usb, None) => None,
        (link, _) => Some(link),
    }
}

/// Send `edit`'s moved rows to the engine, set the remember switch, and save every row.
///
/// # Errors
/// [`Busy`] when the engine takes no command, and then nothing is sent, switched or saved.
pub(crate) fn apply(
    edit: &SettingsEdit,
    snapshot: &Snapshot,
    commands: &mpsc::Sender<Command>,
    settings: &Settings,
) -> Result<Applied, Busy> {
    // Reserve every slot first, so a full queue applies none of the edit rather than part of it.
    // The excluded list is not among them: a front end sends it on its own, through
    // `apply_excluded`, so in an edit it is always what is in force.
    let sends: Vec<Command> = edit
        .pool
        .map(|pool| Command::SetPool { pool })
        .into_iter()
        .chain(edit.planner.map(Command::SetPlanner))
        .chain(edit.fan.map(|duty| Command::SetFanDuty { duty }))
        .chain(edit.bluetooth.map(Command::SetBluetooth))
        .chain(edit.wardrive_esp.map(Command::SetWardriveEsp))
        .chain(edit.sd_log.map(Command::SetSdLog))
        .chain(edit.gps_rate.map(Command::SetGpsRate))
        .collect();
    let Ok(permits) = commands.try_reserve_many(sends.len()) else { return Err(Busy) };
    for (permit, command) in permits.zip(sends) {
        permit.send(command);
    }

    let mut state = settings.lock();
    let remember = (edit.remember_bridge != state.memory.is_enabled()).then(|| {
        state.memory.set_enabled(edit.remember_bridge);
        // Remember the bridge connected now. A reconnect may never come.
        if edit.remember_bridge
            && let Some(bridge) = &snapshot.bridge
            && !state.simulated
        {
            state.memory.remember_on(bridge.mac, None);
        }
        edit.remember_bridge
    });
    let wait_for_fix_moved = edit.wait_for_fix != state.saved.gps.waits_for_fix();
    let saved = state.save_edit(edit, edit.gps_rate.unwrap_or(snapshot.gps_rate_hz));
    Ok(Applied { remember, wait_for_fix_moved, saved })
}

/// Send `ssids` to the engine as the excluded list, and save them. Nothing else in the store
/// changes, and nothing else goes to the engine.
///
/// # Errors
/// [`Busy`] when the engine takes no command, and then nothing is saved.
pub(crate) fn apply_excluded(
    ssids: &[String],
    commands: &mpsc::Sender<Command>,
    settings: &Settings,
) -> Result<Result<String, SaveError>, Busy> {
    let bytes = excluded_ssid_bytes(ssids).collect();
    if commands.try_send(Command::SetExcludedSsids { ssids: bytes }).is_err() {
        return Err(Busy);
    }
    let mut state = settings.lock();
    let mut written = state.saved.clone();
    written.excluded_ssids = ssids.to_vec();
    Ok(state.save(written))
}
