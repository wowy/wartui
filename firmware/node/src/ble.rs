//! Bluetooth scanning, run only while the host has set `ASSIGN_FLAG_BLE` for this
//! node — at most one node in a fleet, and none by default. Bluetooth and Wi-Fi share
//! the one 2.4 GHz antenna, and a controller left free takes it whenever the node is
//! otherwise idle (`docs/phase-0-findings.md`, `docs/phase-1-findings.md`).
//!
//! Two things here keep the two apart, and a third keeps the scan hearing anything.
//!
//! The scan is **bounded and switched off** rather than left running.
//! `HCI_LE_Set_Scan_Enable(0)` stops the controller taking the antenna at all,
//! which is stronger than waiting for a scan to finish: an initialized host stack
//! left behind a finished scan can keep the radio.
//!
//! And the node **sniffs no Wi-Fi at all** while it holds the scan, so there is no
//! sweep for the scan to collide with: it runs one scan after another, back to
//! back, and nothing else wants the antenna: everything the node says goes over the
//! node bus.
//!
//! The scan **needs Wi-Fi awake**. The two share one radio, and the node is a station
//! that never associates, which `esp-radio` lets sleep by default whenever promiscuous
//! mode is off. The Bluetooth node sniffs nothing, so its radio sleeps, and a
//! controller whose radio is asleep acknowledges every command and reports nothing.
//! `main.rs` turns that power management off (`docs/bluetooth-scan-findings.md`).
//! [`Scanner::new`] returns a refused command as an error, and [`Sweep::silent`]
//! counts scans that heard nothing, so `main.rs` can say both on the console.
//!
//! There is no host stack. `esp-radio` exposes the controller as a raw HCI pipe and
//! all this firmware wants is an address and a signal strength, so the packets are
//! built and read by `wartui_proto::hci` and this file is only the conversation.

use esp_hal::peripherals::BT;
use esp_hal::time::{Duration, Instant};
use esp_radio::ble::Config;
use esp_radio::ble::controller::BleConnector;
use esp_rtos::CurrentThreadHandle;
use esp_sync::NonReentrantMutex;
use wartui_proto::dedup::DEDUP_RSSI_GAIN_BLE_DB;
use wartui_proto::hci::{
    AdvReport, PACKET_MAX, RESET, SCAN_UNIT_US, SET_EVENT_MASK, adv_reports, command_complete,
    opcode, set_scan_enable, set_scan_parameters,
};
use wartui_proto::node::ASSIGNED_BEAT_MS;
use wartui_proto::pending::BlePending;

use crate::sniff;

/// How long one scan listens.
pub const SCAN_MS: u32 = 500;

// A scan runs back to back with the next regardless of the heartbeat, so the only
// thing that would be wrong is a scan long enough to swallow a whole heartbeat
// interval and never let the deadline be checked between scans.
const _: () = assert!(
    SCAN_MS < ASSIGNED_BEAT_MS,
    "a scan must finish inside one heartbeat interval, or the beat deadline is never checked"
);

/// Distinct advertisers one sweep will hold.
///
/// Slots go only to advertisers due to be reported: [`Scanner::sweep`] asks
/// [`sniff::SEEN`] before an address takes one, so an advertiser the host already
/// has costs nothing however often it repeats. An ordinary room filled about 60
/// of it (`docs/phase-1-findings.md`), and overflow drops the *newest* advertiser,
/// so the number sits at twice the measurement rather than just above it. The headroom
/// is affordable because [`BlePending`] finds a held address by hash: every report
/// heard is checked against the buffer with interrupts held off, and a linear search
/// of 128 reports costs up to 18 µs there.
/// [`refused`] counts those advertisers, once per scan each, and it is the only
/// sign of saturation: [`Sweep::heard`] is capped here. The heartbeat carries
/// it to the host, which shows the fleet's total in the footer's `buffer full` line.
///
/// It is also the ceiling on the burst: `report_ble` drains the sweep into
/// `Outgoing` batches delivered over the node bus before the next scan starts, so this is
/// the worst-case delay standing between one scan ending and the next one
/// starting.
///
/// The burst is also what bounds this size. A full buffer drains as fast as the bus
/// acknowledges it, and what the bridge cannot forward to USB at once waits in its
/// queue toward the host, which drops its oldest. Measured with
/// 250-byte batches, bursts of 31 batches (512 reports) arrived whole, and loss
/// began at about 46. A batch fills up to 1024 bytes, so 128 reports pack into about
/// two, which leaves most of that queue for the rest of the fleet
/// arriving at the same moment. A lost batch was acknowledged
/// on the bus, so its advertisers are already in [`sniff::SEEN`] and stay hidden
/// until the refresh.
///
/// The buffer lives in a `static`, the way `sniff` holds its sightings, because a
/// report is twelve bytes with the manufacturer identifier: a by-value `Scanner`
/// puts enough of the buffer on the stack of `Scanner::new` and of `main` to trip
/// `clippy::large_stack_frames`, which `main.rs` denies. As a static, the reports and
/// their index cost 2,192 bytes that no stack has to find room for.
const REPORTS: usize = 128;

/// Hash slots behind [`REPORTS`]: a power of two at least twice it, so the index is
/// never more than half full.
const REPORT_INDEX: usize = 2 * REPORTS;

/// The reports of the sweep in flight, and how far the main loop has drained it.
///
/// Its lock is taken before [`sniff::SEEN`]'s, in [`Scanner::sweep`], and never
/// after it.
static PENDING: NonReentrantMutex<BlePending<REPORTS, REPORT_INDEX>> =
    NonReentrantMutex::new(BlePending::new());

/// Take the oldest report not yet taken, if there is one.
///
/// One at a time rather than a bulk drain, so nothing holds the buffer's lock
/// across a delivery — the same shape `sniff` gives its sightings.
pub fn take() -> Option<AdvReport> {
    PENDING.with(BlePending::take)
}

/// Advertisers turned away by a full buffer since boot, each counted once per
/// sweep however often it repeats. Wraps.
pub fn refused() -> u16 {
    PENDING.with(|pending| pending.refused())
}

/// Listen continuously while enabled: interval and window equal, at 30 ms.
const SCAN_WINDOW: u16 = (30_000 / SCAN_UNIT_US) as u16;

/// Granularity of the collection loop.
const POLL_MS: u64 = 2;

/// Why [`Scanner::new`] gave up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartError {
    /// The controller would not come up, or would not take a command.
    Controller,
    /// The controller answered a command with a non-zero status.
    Refused { opcode: u16, status: u8 },
}

/// What one [`Scanner::sweep`] heard.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sweep {
    /// Distinct advertisers due to be reported, at most [`REPORTS`].
    pub heard: usize,
    /// Advertising-report events read, whatever they held.
    pub events: u32,
    /// Sweeps in a row, this one included, that read no advertising-report event.
    pub silent: u32,
}

pub struct Scanner<'d> {
    connector: BleConnector<'d>,
    packet: [u8; PACKET_MAX],
    silent: u32,
}

impl<'d> Scanner<'d> {
    /// Bring the controller up and tell it how to scan. Scanning stays off.
    pub fn new(bt: BT<'d>) -> Result<Self, StartError> {
        let mut scanner = Self {
            connector: BleConnector::new(bt, Config::default())
                .map_err(|_| StartError::Controller)?,
            packet: [0; PACKET_MAX],
            silent: 0,
        };
        // A reset first: the controller keeps whatever state the last run left it
        // in, and enabling a scan twice is an error rather than a no-op.
        scanner.setup(&RESET)?;
        // After the reset, because the reset restores the default mask that
        // hides advertising reports. See `SET_EVENT_MASK`.
        scanner.setup(&SET_EVENT_MASK)?;
        scanner.setup(&set_scan_parameters(SCAN_WINDOW, SCAN_WINDOW))?;
        Ok(scanner)
    }

    /// Send one start-up command, and fail on a refusal.
    fn setup(&mut self, bytes: &[u8]) -> Result<(), StartError> {
        match self.command(bytes)? {
            Some(status) if status != 0 => {
                Err(StartError::Refused { opcode: opcode(bytes).unwrap_or(0), status })
            }
            _ => Ok(()),
        }
    }

    /// Listen for `SCAN_MS`, then stop, filing what was heard in [`PENDING`].
    ///
    /// Returns a [`Sweep`]: how many distinct advertisers due to be reported were
    /// heard, at most [`REPORTS`], how many advertising-report events were read, and
    /// how many sweeps in a row have read none. The reports themselves come out
    /// through [`take`], one at a time, so nothing here holds the buffer's lock across
    /// a sleep or a delivery.
    ///
    /// Distinct addresses only, keeping the strongest reading for each: the same
    /// advertiser is heard several times a second and only one belongs on the wire.
    ///
    /// "Heard" is per call and not quite per window, because the disable at the end
    /// drains on a budget and a busy room leaves a few reports queued for the next
    /// scan to count. Dedup absorbs the repeats, so the cost is the precision of a
    /// console figure; draining first would cost whole reports instead.
    pub fn sweep(&mut self) -> Sweep {
        // A fresh sweep discards nothing the main loop has not already taken:
        // the drain in `report_ble` runs to completion between sweeps.
        PENDING.with(BlePending::clear);
        // Written directly rather than through `command`, because from here on the
        // queue is what the sweep is for: `command` drains until two reads come
        // back empty, and a busy room can hold it there for its whole budget,
        // discarding every advertising report that arrives in it. The collect loop
        // below absorbs the Command Complete at no cost, since `adv_reports` yields
        // nothing for a packet that is not one.
        if self.connector.write(&set_scan_enable(true)).is_err() {
            self.silent = self.silent.saturating_add(1);
            return Sweep { silent: self.silent, ..Sweep::default() };
        }

        // `SEEN` changes only when `report_ble` records a delivery, after the
        // sweep, so one reading of the clock serves every report in it.
        let now = sniff::now_ms();
        let until = Instant::now() + Duration::from_millis(u64::from(SCAN_MS));
        let mut events = 0u32;
        while Instant::now() < until {
            match self.connector.next(&mut self.packet) {
                Ok(0) | Err(_) => CurrentThreadHandle::get().delay(Duration::from_millis(POLL_MS)),
                Ok(read) => {
                    let mut reports = adv_reports(&self.packet[..read]).peekable();
                    if reports.peek().is_some() {
                        events = events.saturating_add(1);
                    }
                    for report in reports {
                        PENDING.with(|pending| {
                            pending.record(report, |r| {
                                sniff::SEEN.with(|seen| {
                                    seen.is_due(
                                        &r.address,
                                        Some(r.rssi),
                                        DEDUP_RSSI_GAIN_BLE_DB,
                                        now,
                                    )
                                })
                            });
                        });
                    }
                }
            }
        }

        // Unconditionally, and before anything else happens. This is the line
        // the whole module exists for. Its answer changes nothing: there is no
        // other way to stop the scan.
        let _ = self.command(&set_scan_enable(false));
        self.silent = if events == 0 { self.silent.saturating_add(1) } else { 0 };
        Sweep { heard: PENDING.with(|pending| pending.len()), events, silent: self.silent }
    }

    /// Send one command, drain whatever the controller says back, and return the
    /// status of the Command Complete answering it.
    ///
    /// `None` when no Command Complete for its opcode arrived inside the budget,
    /// which is not a refusal. `Err` when the command could not be written. Reading
    /// the queue is also what stops a stale completion occupying it across a scan.
    /// This is about queue space, not misparsing — `adv_reports` yields nothing for a
    /// packet that is not a report — which is why `sweep` starts its scan without
    /// coming through here.
    ///
    /// A command can be answered with both a Command Status and a Command Complete,
    /// so stopping at the first packet leaves one behind for the next `sweep`. It
    /// ends on a quiet queue or on a budget: a controller that will not stop talking
    /// must not hold the sweep.
    fn command(&mut self, bytes: &[u8]) -> Result<Option<u8>, StartError> {
        self.connector.write(bytes).map_err(|_| StartError::Controller)?;
        let sent = opcode(bytes);
        let mut answer = None;
        let mut quiet = 0;
        for _ in 0..32 {
            CurrentThreadHandle::get().delay(Duration::from_millis(POLL_MS));
            match self.connector.next(&mut self.packet) {
                // Two quiet reads rather than one: the first can simply be
                // ahead of a controller that has not answered yet.
                Ok(0) | Err(_) if quiet >= 1 => return Ok(answer),
                Ok(0) | Err(_) => quiet += 1,
                Ok(read) => {
                    quiet = 0;
                    if let Some((op, status)) = command_complete(&self.packet[..read])
                        && Some(op) == sent
                    {
                        answer = Some(status);
                    }
                }
            }
        }
        Ok(answer)
    }
}
