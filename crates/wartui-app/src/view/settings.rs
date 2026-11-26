//! The settings in force, as a settings screen seeds its rows, and the bridge's answers to a
//! power-cycle and a format.
//!
//! **The bridge's settings are what its last `Status` said, and nothing while the link is down.**
//! A value sent then would be dropped rather than stored, so a screen shows no value to step. The
//! Bluetooth scan is the exception: the run holds it, and starts the scan in that mode once the
//! bridge reports it stopped, so it is `None` only while the scan is reported stopped.
//!
//! **The SD card is what a screen decides its format row from**, typed: whether it can be formatted
//! now is decided here, and the words for each case are the front end's.

use wartui_core::engine::Snapshot;
use wartui_proto::fleet::ScanMode;
use wartui_proto::link::{PowerOutcome, SdFormatOutcome, SdLog, SdLogState, SdState, SdStatus};
use wartui_proto::plan::{ChannelPool, PlanSettings, Planner, SparseNodes};

use super::SlotShown;

/// The settings in force, as a settings screen seeds its rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsShown {
    /// The channel pool the bridge holds. `None` with the link down or before its first `Status`.
    pub pool: Option<ChannelPool>,
    /// The planner the bridge deals with. `None` likewise.
    pub planner: Option<PlannerShown>,
    /// The lid fan's duty, in percent. `None` likewise, and with no carrier.
    pub fan_duty_pct: Option<u8>,
    /// Whether a phone may link as a WardriveESP board. `None` likewise.
    pub wardrive_esp: Option<bool>,
    /// Whether the bridge logs to its SD card. `None` likewise, and with no card slot.
    pub sd_log: Option<bool>,
    /// Whether Bluetooth is in the scan. `None` while the scan is stopped: nothing to toggle.
    pub bluetooth: Option<bool>,
    /// The bridge receiver's rate, in hertz, as the engine holds it.
    pub gps_rate_hz: u8,
    /// The Wi-Fi networks never captured.
    pub excluded_ssids: Vec<String>,
    /// The bridge's SD card, and whether it can be formatted now.
    pub sd: SdShown,
}

/// The planner and its sparse node count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannerShown {
    /// The deal.
    pub planner: Planner,
    /// The sparse nodes the weighted deal asks for, 1 to `SparseNodes::MAX`, or `None` for
    /// automatic.
    pub sparse_nodes: Option<u8>,
}

impl From<PlanSettings> for PlannerShown {
    fn from(settings: PlanSettings) -> Self {
        let byte = settings.sparse_nodes.byte();
        Self { planner: settings.planner, sparse_nodes: (byte != 0).then_some(byte) }
    }
}

impl From<PlannerShown> for PlanSettings {
    /// The settings `shown` names. A count outside 1 to `SparseNodes::MAX` reads as automatic.
    fn from(shown: PlannerShown) -> Self {
        let sparse_nodes =
            shown.sparse_nodes.and_then(SparseNodes::fixed).unwrap_or(SparseNodes::AUTO);
        Self { planner: shown.planner, sparse_nodes }
    }
}

/// The bridge's SD card, first match first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdShown {
    /// The link is down.
    NoBridge,
    /// The bridge has sent no `Status` yet.
    Waiting,
    /// The bridge has no card slot.
    NoSlot,
    /// The slot, and the card in it if any.
    Card {
        /// What the card holds.
        state: SdState,
        /// Its size in MiB, 0 with no card.
        card_mb: u32,
        /// The volume's free space in MiB, when it is ready and says.
        free_mb: Option<u32>,
        /// The log the bridge writes to it.
        log: SdLogShown,
        /// A format can be asked for: a card that is not FAT32, is ready, or failed. None while a
        /// format runs or with no card.
        can_format: bool,
    },
}

/// The log the bridge writes to its SD card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SdLogShown {
    /// The setting.
    pub on: bool,
    /// What the writer is doing.
    pub state: SdLogState,
    /// The number of the file open, `LOG/NNNNNNNN.BIN`, or `None` with none open.
    pub file: Option<u32>,
    /// Bytes written since the bridge booted.
    pub written: u64,
    /// Records dropped since boot, because the log's ring filled first.
    pub dropped: u32,
    /// Card transfers that failed since boot.
    pub errors: u32,
    /// The longest synchronous stretch of one writer step, in microseconds.
    pub busy_max_us: u32,
}

impl From<SdLog> for SdLogShown {
    fn from(log: SdLog) -> Self {
        Self {
            on: log.on,
            state: log.state,
            file: log.file,
            written: log.written,
            dropped: log.dropped,
            errors: log.errors,
            busy_max_us: log.busy_max_us,
        }
    }
}

/// The bridge's latest answers, each for a front end to show once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answers {
    /// The last answer to a power-cycle.
    pub power: Option<PowerAnswered>,
    /// The last answer to a format of the SD card.
    pub sd: Option<SdAnswered>,
}

/// The bridge's answer to a power-cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerAnswered {
    /// Counts the answers, from 1, so a front end tells a new one from the last.
    pub id: u64,
    /// The slot answered for, labeled on the board that carried the nodes as it arrived.
    pub slot: SlotShown,
    /// What happened.
    pub outcome: PowerOutcome,
}

/// The bridge's answer to a format of its SD card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SdAnswered {
    /// Counts the answers, from 1, so a front end tells a new one from the last.
    pub id: u64,
    /// What happened.
    pub outcome: SdFormatOutcome,
}

/// The settings in force in `snapshot`.
pub(super) fn of(snapshot: &Snapshot) -> SettingsShown {
    let up = snapshot.link_up;
    SettingsShown {
        pool: snapshot.pool.filter(|_| up),
        planner: snapshot.planner.filter(|_| up).map(PlannerShown::from),
        fan_duty_pct: snapshot
            .bridge_status
            .and_then(|status| status.fan)
            .map(|fan| fan.duty.get())
            .filter(|_| up),
        wardrive_esp: snapshot.wardrive_esp.filter(|_| up),
        sd_log: snapshot.sd_log.filter(|_| up),
        // Before the first report the scan is off, and the engine starts it in the mode asked for
        // meanwhile.
        bluetooth: match snapshot.scan {
            Some(ScanMode::Stopped) => None,
            scan => Some(scan == Some(ScanMode::WifiBluetooth)),
        },
        gps_rate_hz: snapshot.gps_rate_hz,
        excluded_ssids: snapshot.excluded_ssids.clone(),
        sd: sd(snapshot),
    }
}

/// The SD card: none with the link down, before the first `Status`, or with no slot.
fn sd(snapshot: &Snapshot) -> SdShown {
    if !snapshot.link_up {
        return SdShown::NoBridge;
    }
    let Some(status) = snapshot.bridge_status else { return SdShown::Waiting };
    let Some(SdStatus { state, card_mb, free_mb, log }) = status.sd else {
        return SdShown::NoSlot;
    };
    let can_format = matches!(state, SdState::NoFat32 | SdState::Ready | SdState::Failed);
    SdShown::Card { state, card_mb, free_mb, log: log.into(), can_format }
}

/// The bridge's latest answers in `snapshot`.
pub(super) fn answers(snapshot: &Snapshot) -> Answers {
    Answers {
        power: snapshot.power_answer.map(|answer| PowerAnswered {
            id: answer.id,
            slot: SlotShown {
                index: answer.slot.as_u8(),
                label: answer.board.label(answer.slot).to_string(),
            },
            outcome: answer.outcome,
        }),
        sd: snapshot.sd_answer.map(|answer| SdAnswered { id: answer.id, outcome: answer.outcome }),
    }
}
