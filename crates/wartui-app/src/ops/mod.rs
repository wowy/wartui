//! What a front end asks of a bridge outside a capture, over a link it opened: a status read, a
//! setting sent and read back, the pairing window, the bonds cleared, a node power-cycled, the SD
//! card formatted, the scan started and its plan watched.
//!
//! **Typed, and printing nothing.** Every wait ends in a [`LinkError`] when the link goes down,
//! closes, cannot queue a command or the answer does not come in time, and the front end words
//! it. A bridge this host cannot read past the frozen core refuses every command here alike
//! ([`LinkError::Incompatible`], [`NotReady::Incompatible`]). A fault the bridge reports while a wait runs, an `Error` message such as a P4 updating its
//! radio, is handed to the caller's `fault` as it arrives: the bridge still answers, so a fault
//! never ends a wait.
//!
//! **A setting is sent, given time to land, then read back.** The bridge applies a setting and
//! keeps it on its own flash; a fresh `Status` is what says it took. Each operation's settle time
//! is defined once here, with its reason. Choosing and opening the link stays the front end's.
//!
//! **A link held between captures is parked** ([`park`]), read so nothing piles up in it.

mod park;
mod ready;

use std::time::Duration;

use tokio::time::error::Elapsed;
use wartui_bridge::{BridgeCore, LinkEvent, LinkHandle, SendError};
use wartui_proto::bus::{MAX_SLOTS, Slot};
use wartui_proto::fan::{FanDuty, FanStats, TACH_WINDOW_MS};
use wartui_proto::fleet::{Member, Report, ScanMode};
use wartui_proto::heapless;
use wartui_proto::link::{
    BluetoothStatus, BridgeHealth, BridgeToHost, HostToBridge, PowerOutcome, QueueStats,
    RadioLinkStats, SdFormatOutcome, SdStatus,
};
use wartui_proto::plan::{ChannelPool, PlanSettings};

pub use park::{ParkEnded, Parked, park};
pub use ready::{NotReady, ReadyNotice, ReadyWaited, ready};

use crate::settings::Link;

/// How long the bridge has to answer a request. Generous enough for a bridge that is busy
/// forwarding a fleet, short enough that a wedged one is reported rather than waited on.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait for the `Fleet` report the bridge sends behind a `Status`.
pub const PLAN_AFTER: Duration = Duration::from_secs(1);

/// How long a new fan duty runs before its speed is read: two tach windows, so one whole window
/// ran at it.
pub const FAN_SETTLE: Duration = Duration::from_millis(2 * TACH_WINDOW_MS as u64);

/// How long a setting the bridge stores (the pool, the SD log, `wardrive esp`) is listened on for
/// a fault before it is read back.
pub const SETTING_SETTLE: Duration = Duration::from_millis(500);

/// How long clearing the bonds is listened on for a fault before they are read back.
pub const BONDS_SETTLE: Duration = Duration::from_secs(1);

/// How long a started scan's reports are read once the bridge shows the mode: an idle node
/// heartbeats every second, so every node present has joined the plan by then.
pub const PLAN_SETTLE: Duration = Duration::from_secs(3);

/// How long the bridge has to answer a power-cycle. The worst case, on a carrier whose node supply
/// is out, is about 5 s: a cycle already running ahead of it (1 s off and its tries at power-on),
/// up to 14 nodes powered 100 ms apart once the request ends the scouting, and its own 1 s off.
/// The rest is room, though not for a queue of several requests ahead of it.
pub const POWER_CYCLE_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the bridge has to finish a format: room past the few seconds a large card takes. The
/// bridge answers once the format ends, and the transport keeps the link alive meanwhile.
pub const FORMAT_TIMEOUT: Duration = Duration::from_secs(120);

/// What a command that could not be queued asked the bridge for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Status,
    FanDuty,
    Pool,
    SdLog,
    WardriveEsp,
    OpenPairing,
    ClearBonds,
    PowerCycle,
    FormatSd,
    StartScan,
}

/// Why a read of the link ended without its answer.
#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    /// The link went down, with the transport's reason, which is already the operator's text.
    #[error("{0}")]
    Down(String),
    /// The transport ended.
    #[error("the link closed")]
    Closed,
    /// The answer did not come in time.
    #[error(transparent)]
    NoAnswer(Elapsed),
    /// A command could not be queued.
    #[error("{error}")]
    Queue { request: Request, error: SendError },
    /// The bridge shares only the frozen core with this host, so it takes no command here.
    #[error("the bridge {} shares only the frozen core with this host", .0.version)]
    Incompatible(BridgeCore),
}

impl LinkError {
    /// A command for `request` that `error` kept from being queued: [`Self::Incompatible`] for a
    /// bridge this host cannot read, else [`Self::Queue`].
    #[must_use]
    pub fn queueing(request: Request, error: SendError) -> Self {
        match error {
            SendError::Incompatible(core) => Self::Incompatible(core),
            error => Self::Queue { request, error },
        }
    }
}

/// A bridge's `Status`, its fields as the message carries them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub rx_count: u32,
    pub dropped_tx: u32,
    pub uptime_ms: u32,
    pub rx_oversize: u32,
    pub queues: QueueStats,
    pub radio_link: RadioLinkStats,
    pub host_uart_errors: u32,
    pub fan: Option<FanStats>,
    pub pool: ChannelPool,
    pub planner: PlanSettings,
    pub scan: ScanMode,
    pub ble: Option<Slot>,
    pub present: u32,
    pub members: heapless::Vec<Member, MAX_SLOTS>,
    pub sd: Option<SdStatus>,
    pub bluetooth: BluetoothStatus,
    pub health: BridgeHealth,
}

impl Status {
    /// `message`'s fields when it is a `Status`.
    #[must_use]
    pub fn from_message(message: BridgeToHost) -> Option<Self> {
        match message {
            BridgeToHost::Status {
                rx_count,
                dropped_tx,
                uptime_ms,
                rx_oversize,
                queues,
                radio_link,
                host_uart_errors,
                fan,
                pool,
                planner,
                scan,
                ble,
                present,
                members,
                sd,
                bluetooth,
                health,
            } => Some(Self {
                rx_count,
                dropped_tx,
                uptime_ms,
                rx_oversize,
                queues,
                radio_link,
                host_uart_errors,
                fan,
                pool,
                planner,
                scan,
                ble,
                present,
                members,
                sd,
                bluetooth,
                health,
            }),
            _ => None,
        }
    }
}

/// A setting sent and read back: the status after, and whether it holds what was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    pub status: Status,
    pub taken: bool,
}

/// How clearing the bonds ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cleared {
    /// Over USB: the status after, which shows whether the bonds went.
    Status(Box<Status>),
    /// Over Bluetooth this host's bond went with the others, and the bridge disconnected it, or
    /// the wait ran out first. Either is the expected end.
    Disconnected,
}

/// What a started scan reported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Seen {
    /// Its mode, from a `Status` or a `Fleet` report.
    pub mode: Option<ScanMode>,
    /// The node it gave Bluetooth, from the same.
    pub pick: Option<Slot>,
    /// Its last `Fleet` report.
    pub plan: Option<Report>,
    /// The planner its last `Status` reported.
    pub planner: Option<PlanSettings>,
}

impl Seen {
    /// Take in one message.
    fn note(&mut self, message: &BridgeToHost) {
        match message {
            BridgeToHost::Status { scan, ble, planner, .. } => {
                self.mode = Some(*scan);
                self.pick = *ble;
                self.planner = Some(*planner);
            }
            BridgeToHost::Fleet(report) => {
                self.mode = Some(report.mode);
                self.pick = report.shares.iter().find(|share| share.ble).map(|share| share.slot);
                self.plan = Some(report.clone());
            }
            _ => {}
        }
    }
}

/// Queue `command` in the bulk queue, for `request`.
fn send(link: &LinkHandle, command: HostToBridge, request: Request) -> Result<(), LinkError> {
    link.send_bulk(command).map_err(|error| LinkError::queueing(request, error))
}

/// One link event while a wait runs: the message to look at, a fault handed over, or the end.
fn message(
    event: Option<LinkEvent>,
    fault: &mut dyn FnMut(&str),
) -> Result<Option<BridgeToHost>, LinkError> {
    match event {
        Some(LinkEvent::Message(BridgeToHost::Error { message })) => {
            fault(&message);
            Ok(None)
        }
        Some(LinkEvent::Message(message)) => Ok(Some(message)),
        Some(LinkEvent::Disconnected { reason, .. }) => Err(LinkError::Down(reason)),
        Some(_) => Ok(None),
        None => Err(LinkError::Closed),
    }
}

/// Read the link for `wait`, handing over each fault, so a command just sent lands before
/// anything is read back.
///
/// # Errors
/// The link went down or closed.
pub async fn settle(
    link: &mut LinkHandle,
    wait: Duration,
    fault: &mut dyn FnMut(&str),
) -> Result<(), LinkError> {
    let until = tokio::time::Instant::now() + wait;
    loop {
        let Ok(event) = tokio::time::timeout_at(until, link.recv()).await else { return Ok(()) };
        message(event, fault)?;
    }
}

/// Read the link until `take` takes a message, handing over each fault meanwhile. The caller's
/// timeout bounds it.
///
/// # Errors
/// The link went down or closed.
pub async fn answer<T>(
    link: &mut LinkHandle,
    fault: &mut dyn FnMut(&str),
    mut take: impl FnMut(BridgeToHost) -> Option<T>,
) -> Result<T, LinkError> {
    loop {
        if let Some(taken) = message(link.recv().await, fault)?.and_then(&mut take) {
            return Ok(taken);
        }
    }
}

/// Ask for a `Status` and wait up to `within` for it, handing over each fault meanwhile.
///
/// # Errors
/// The request could not be queued, the link went down or closed, or no answer came in time.
pub async fn status(
    link: &mut LinkHandle,
    within: Duration,
    fault: &mut dyn FnMut(&str),
) -> Result<Status, LinkError> {
    send(link, HostToBridge::GetStatus, Request::Status)?;
    tokio::time::timeout(within, answer(link, fault, Status::from_message))
        .await
        .map_err(LinkError::NoAnswer)?
}

/// [`status`], then the `Fleet` report the bridge sends behind it, or the last one before it when
/// none follows within [`PLAN_AFTER`]. `within` bounds the wait for the `Status`; the plan wait
/// has its own [`PLAN_AFTER`] cap, so a `Status` that comes late is kept.
///
/// # Errors
/// As [`status`].
pub async fn status_and_plan(
    link: &mut LinkHandle,
    within: Duration,
    fault: &mut dyn FnMut(&str),
) -> Result<(Status, Option<Report>), LinkError> {
    send(link, HostToBridge::GetStatus, Request::Status)?;
    let mut plan = None;
    let answered = answer(link, fault, |message| match message {
        BridgeToHost::Fleet(report) => {
            plan = Some(report);
            None
        }
        message => Status::from_message(message),
    });
    let status = tokio::time::timeout(within, answered).await.map_err(LinkError::NoAnswer)??;
    let deadline = tokio::time::Instant::now() + PLAN_AFTER;
    loop {
        let Ok(event) = tokio::time::timeout_at(deadline, link.recv()).await else {
            return Ok((status, plan));
        };
        if let Some(BridgeToHost::Fleet(report)) = message(event, fault)? {
            return Ok((status, Some(report)));
        }
    }
}

/// Send `command`, let it land for `settle`, then read the status back and judge whether it took.
async fn set(
    link: &mut LinkHandle,
    command: HostToBridge,
    request: Request,
    wait: Duration,
    fault: &mut dyn FnMut(&str),
    took: impl FnOnce(&Status) -> bool,
) -> Result<Setting, LinkError> {
    send(link, command, request)?;
    settle(link, wait, fault).await?;
    let status = status(link, REPLY_TIMEOUT, fault).await?;
    Ok(Setting { taken: took(&status), status })
}

/// Set the carrier's lid fan to `duty`, and read back the duty and the speed at it.
///
/// # Errors
/// As [`status`].
pub async fn set_fan(
    link: &mut LinkHandle,
    duty: FanDuty,
    fault: &mut dyn FnMut(&str),
) -> Result<Setting, LinkError> {
    let command = HostToBridge::SetFan { duty };
    set(link, command, Request::FanDuty, FAN_SETTLE, fault, |status| {
        status.fan.map(|fan| fan.duty) == Some(duty)
    })
    .await
}

/// Set the channel pool the bridge's planner deals, and read it back.
///
/// # Errors
/// As [`status`].
pub async fn set_pool(
    link: &mut LinkHandle,
    pool: ChannelPool,
    fault: &mut dyn FnMut(&str),
) -> Result<Setting, LinkError> {
    let command = HostToBridge::SetPool { pool };
    set(link, command, Request::Pool, SETTING_SETTLE, fault, |status| status.pool == pool).await
}

/// Set whether the bridge logs to its microSD card, and read it back.
///
/// # Errors
/// As [`status`].
pub async fn set_sd_log(
    link: &mut LinkHandle,
    on: bool,
    fault: &mut dyn FnMut(&str),
) -> Result<Setting, LinkError> {
    let command = HostToBridge::SetSdLog { on };
    set(link, command, Request::SdLog, SETTING_SETTLE, fault, |status| {
        status.sd.map(|sd| sd.log.on) == Some(on)
    })
    .await
}

/// Set whether phones may link to the bridge as a WardriveESP board, and read it back.
///
/// # Errors
/// As [`status`].
pub async fn set_wardrive_esp(
    link: &mut LinkHandle,
    on: bool,
    fault: &mut dyn FnMut(&str),
) -> Result<Setting, LinkError> {
    let command = HostToBridge::SetWardriveEsp { on };
    set(link, command, Request::WardriveEsp, SETTING_SETTLE, fault, |status| {
        status.bluetooth.wardrive_esp == on
    })
    .await
}

/// Open the bridge's pairing window, and read back its Bluetooth host link.
///
/// # Errors
/// As [`status`].
pub async fn open_pairing(
    link: &mut LinkHandle,
    fault: &mut dyn FnMut(&str),
) -> Result<Status, LinkError> {
    send(link, HostToBridge::OpenPairing, Request::OpenPairing)?;
    status(link, REPLY_TIMEOUT, fault).await
}

/// Clear every Bluetooth bond the bridge keeps. Over USB a status shows them gone. Over
/// Bluetooth this host's bond goes with them and the bridge disconnects it, so that disconnect is
/// the expected end, and only a fault is listened for until it, or for [`BONDS_SETTLE`].
///
/// # Errors
/// Over USB, as [`status`]; over Bluetooth, only a command that could not be queued.
pub async fn clear_bonds(
    link: &mut LinkHandle,
    over: Link,
    fault: &mut dyn FnMut(&str),
) -> Result<Cleared, LinkError> {
    send(link, HostToBridge::ClearBonds, Request::ClearBonds)?;
    if over == Link::Bluetooth {
        let until = tokio::time::Instant::now() + BONDS_SETTLE;
        loop {
            match tokio::time::timeout_at(until, link.recv()).await {
                Ok(Some(LinkEvent::Message(BridgeToHost::Error { message }))) => fault(&message),
                Ok(Some(LinkEvent::Disconnected { .. }) | None) | Err(_) => {
                    return Ok(Cleared::Disconnected);
                }
                Ok(Some(_)) => {}
            }
        }
    }
    settle(link, BONDS_SETTLE, fault).await?;
    Ok(Cleared::Status(Box::new(status(link, REPLY_TIMEOUT, fault).await?)))
}

/// Power-cycle the node in `slot` through the carrier, and wait up to [`POWER_CYCLE_TIMEOUT`] for
/// the bridge's answer.
///
/// # Errors
/// The command could not be queued, the link went down or closed, or no answer came in time.
pub async fn power_cycle(
    link: &mut LinkHandle,
    slot: Slot,
    fault: &mut dyn FnMut(&str),
) -> Result<PowerOutcome, LinkError> {
    send(link, HostToBridge::PowerCycle { slot }, Request::PowerCycle)?;
    let answered = answer(link, fault, |message| match message {
        BridgeToHost::PowerCycled { slot: answered, outcome } if answered == slot => Some(outcome),
        _ => None,
    });
    tokio::time::timeout(POWER_CYCLE_TIMEOUT, answered).await.map_err(LinkError::NoAnswer)?
}

/// Format the bridge's microSD card, and wait up to [`FORMAT_TIMEOUT`] for it to end.
///
/// # Errors
/// As [`power_cycle`].
pub async fn format_sd(
    link: &mut LinkHandle,
    fault: &mut dyn FnMut(&str),
) -> Result<SdFormatOutcome, LinkError> {
    send(link, HostToBridge::FormatSd, Request::FormatSd)?;
    let answered = answer(link, fault, |message| match message {
        BridgeToHost::SdFormatted { outcome } => Some(outcome),
        _ => None,
    });
    tokio::time::timeout(FORMAT_TIMEOUT, answered).await.map_err(LinkError::NoAnswer)?
}

/// Start the scan, wait up to [`REPLY_TIMEOUT`] for the bridge to report it, then read its
/// reports for [`PLAN_SETTLE`] as nodes join. `None` when it did not report the scan started.
///
/// # Errors
/// A command could not be queued, or the link went down or closed.
pub async fn start_scan(
    link: &mut LinkHandle,
    bluetooth: bool,
    fault: &mut dyn FnMut(&str),
) -> Result<Option<Seen>, LinkError> {
    link.send_urgent(HostToBridge::StartScan { bluetooth })
        .map_err(|error| LinkError::queueing(Request::StartScan, error))?;
    send(link, HostToBridge::GetStatus, Request::Status)?;
    let mode = if bluetooth { ScanMode::WifiBluetooth } else { ScanMode::Wifi };
    let mut seen = Seen::default();
    if !watch(link, REPLY_TIMEOUT, &mut seen, fault, |seen| seen.mode == Some(mode)).await? {
        return Ok(None);
    }
    watch(link, PLAN_SETTLE, &mut seen, fault, |_| false).await?;
    Ok(Some(seen))
}

/// Read the link for up to `wait`, noting each report in `seen`, until `done` holds, handing over
/// each fault meanwhile. `Ok(false)` when the wait ran out first.
async fn watch(
    link: &mut LinkHandle,
    wait: Duration,
    seen: &mut Seen,
    fault: &mut dyn FnMut(&str),
    done: impl Fn(&Seen) -> bool,
) -> Result<bool, LinkError> {
    let deadline = tokio::time::Instant::now() + wait;
    while !done(seen) {
        let Ok(event) = tokio::time::timeout_at(deadline, link.recv()).await else {
            return Ok(false);
        };
        if let Some(message) = message(event, fault)? {
            seen.note(&message);
        }
    }
    Ok(true)
}
