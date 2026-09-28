//! Bluetooth scanning, run only while the core has set `ADMIN_FLAG_BLE` for this
//! node — at most one node in a fleet, and none by default. On a stock node BLE
//! cost it every channel assignment sent to it, because an 802.11 acknowledgement
//! comes from the receiver's MAC hardware and its absence means the radio was
//! simply not on the channel: NimBLE and Wi-Fi share the one 2.4 GHz antenna, and
//! the admin window is precisely when the node is otherwise idle and the
//! controller is free to take it. This firmware acknowledged every time at around
//! a tenth of its sweep period
//! (`docs/phase-0-findings.md`, `docs/phase-1-findings.md`).
//!
//! Two things here are what avoid repeating that.
//!
//! The scan is **bounded and switched off** rather than left running.
//! `HCI_LE_Set_Scan_Enable(0)` stops the controller taking the antenna at all,
//! which is stronger than waiting for a scan to finish: an initialised host stack
//! left behind a finished scan can keep the radio.
//!
//! And the node **sniffs no Wi-Fi at all** while it holds the scan, so there is no
//! sweep for the scan to collide with: it runs one scan after another, back to
//! back, and the only thing that wants the antenna is that node's own heartbeat —
//! sent, at most once every [`wartui_proto::plan::ASSIGNED_BEAT_MS`], after a scan
//! has finished rather than during one.
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
use wartui_proto::hci::{
    AdvReport, BlePending, PACKET_MAX, RESET, SCAN_UNIT_US, SET_EVENT_MASK, adv_reports,
    set_scan_enable, set_scan_parameters,
};
use wartui_proto::plan::ASSIGNED_BEAT_MS;

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
/// of it (`docs/phase-1-findings.md`), and overflow drops the *newest* advertiser
/// against a counter no host reads — the one failure here nothing would notice —
/// so the number sits well clear of the measurement rather than just above it.
/// That counter, [`dropped`], counts packets turned away rather than advertisers,
/// and it is the only sign of saturation: the console's `heard` is capped here.
///
/// It is also the ceiling on the burst: `report_ble` drains the sweep into
/// `Outgoing` batches unicast to the core before the next scan starts, so this is
/// the worst-case delay standing between one scan ending and the next one
/// starting.
///
/// The buffer lives in a `static`, the way `sniff` holds its sightings, because a
/// report is twelve bytes with the manufacturer identifier: a by-value `Scanner`
/// puts enough of the buffer on the stack of `Scanner::new` and of `main` to trip
/// `clippy::large_stack_frames`, which `main.rs` denies. In `.bss` the same
/// eighty reports cost 960 bytes that no stack has to find room for.
const REPORTS: usize = 80;

/// The reports of the sweep in flight, and how far the main loop has drained it.
///
/// Its lock is taken before [`sniff::SEEN`]'s, in [`Scanner::sweep`], and never
/// after it.
static PENDING: NonReentrantMutex<BlePending<REPORTS>> = NonReentrantMutex::new(BlePending::new());

/// Take the oldest report not yet taken, if there is one.
///
/// One at a time rather than a bulk drain, so nothing holds the buffer's lock
/// across a transmit — the same shape `sniff` gives its sightings.
pub fn take() -> Option<AdvReport> {
    PENDING.with(BlePending::take)
}

/// Reports turned away by a full buffer since boot. Packets, not advertisers:
/// an address that finds no room counts again on every repeat within a sweep.
pub fn dropped() -> u32 {
    PENDING.with(|pending| pending.dropped())
}

/// Listen continuously while enabled: interval and window equal, at 30 ms.
const SCAN_WINDOW: u16 = (30_000 / SCAN_UNIT_US) as u16;

/// Granularity of the collection loop.
const POLL_MS: u64 = 2;

pub struct Scanner<'d> {
    connector: BleConnector<'d>,
    packet: [u8; PACKET_MAX],
}

impl<'d> Scanner<'d> {
    /// Bring the controller up and tell it how to scan. Scanning stays off.
    pub fn new(bt: BT<'d>) -> Option<Self> {
        let mut scanner = Self {
            connector: BleConnector::new(bt, Config::default()).ok()?,
            packet: [0; PACKET_MAX],
        };
        // A reset first: the controller keeps whatever state the last run left it
        // in, and enabling a scan twice is an error rather than a no-op.
        scanner.command(&RESET)?;
        // After the reset, because the reset restores the default mask that
        // hides advertising reports. See `SET_EVENT_MASK`.
        scanner.command(&SET_EVENT_MASK)?;
        scanner.command(&set_scan_parameters(SCAN_WINDOW, SCAN_WINDOW))?;
        Some(scanner)
    }

    /// Listen for `SCAN_MS`, then stop, filing what was heard in [`PENDING`].
    ///
    /// Returns how many distinct advertisers due to be reported were heard, at
    /// most [`REPORTS`]; the reports themselves come out through [`take`], one at
    /// a time, so nothing here holds the buffer's lock across a sleep or a transmit.
    ///
    /// Distinct addresses only, keeping the strongest reading for each: the same
    /// advertiser is heard several times a second and only one belongs on the wire.
    ///
    /// "Heard" is per call and not quite per window, because the disable at the end
    /// drains on a budget and a busy room leaves a few reports queued for the next
    /// scan to count. Dedup absorbs the repeats, so the cost is the precision of a
    /// console figure; draining first would cost whole reports instead.
    pub fn sweep(&mut self) -> usize {
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
            return 0;
        }

        // `SEEN` changes only when `report_ble` records a delivery, after the
        // sweep, so one reading of the clock serves every report in it.
        let now = sniff::now_ms();
        let until = Instant::now() + Duration::from_millis(u64::from(SCAN_MS));
        while Instant::now() < until {
            match self.connector.next(&mut self.packet) {
                Ok(0) | Err(_) => CurrentThreadHandle::get().delay(Duration::from_millis(POLL_MS)),
                Ok(read) => {
                    for report in adv_reports(&self.packet[..read]) {
                        PENDING.with(|pending| {
                            pending.record(report, |r| {
                                sniff::SEEN.with(|seen| seen.is_due(&r.address, Some(r.rssi), now))
                            });
                        });
                    }
                }
            }
        }

        // Unconditionally, and before anything else happens. This is the line
        // the whole module exists for.
        self.command(&set_scan_enable(false));
        PENDING.with(|pending| pending.len())
    }

    /// Send one command and drain whatever the controller says back.
    ///
    /// The completion event is not inspected: there is nothing useful to do with a
    /// controller that refuses `HCI_Reset`, and reading the queue is what stops a
    /// stale completion occupying it across a scan. This is about queue space, not
    /// misparsing — `adv_reports` yields nothing for a packet that is not a report —
    /// which is why `sweep` starts its scan without coming through here.
    ///
    /// A command can be answered with both a Command Status and a Command Complete,
    /// so stopping at the first packet leaves one behind for the next `sweep`. It
    /// ends on a quiet queue or on a budget: a controller that will not stop talking
    /// must not hold the sweep.
    fn command(&mut self, bytes: &[u8]) -> Option<()> {
        self.connector.write(bytes).ok()?;
        let mut quiet = 0;
        for _ in 0..32 {
            CurrentThreadHandle::get().delay(Duration::from_millis(POLL_MS));
            match self.connector.next(&mut self.packet) {
                // Two quiet reads rather than one: the first can simply be
                // ahead of a controller that has not answered yet.
                Ok(0) | Err(_) if quiet >= 1 => return Some(()),
                Ok(0) | Err(_) => quiet += 1,
                Ok(_) => quiet = 0,
            }
        }
        Some(())
    }
}
