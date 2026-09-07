//! Bluetooth scanning, when this build was asked for it and the core asked for
//! it too.
//!
//! Compiled in only under `--features ble`, and *run* only while the core has
//! set `ADMIN_FLAG_BLE` for this node — at most one node in a fleet, and none
//! by default. Both halves are off for good reason. On a stock node the
//! per-sweep BLE scan costs roughly as much airtime as every channel dwell put
//! together — sixteen sweeps against nine over the same 170 seconds — and, far
//! worse, a node with BLE on acknowledged none of the thirty-two channel
//! assignments sent to it while a node without BLE acknowledged both of its
//! two. Both figures are measured; see `docs/phase-0-findings.md`. This code has
//! since been run three times against that same board on the same bench: it
//! acknowledged its assignment every time on the first attempt, in 5.8 ms, then
//! 6.6 ms, then 5.8 ms, and cost 10.0%, 7.6% and 9.6% of its sweep period rather
//! than the vendor's ~78%. The two runs that measured it against a second board
//! at the same moment are the 10.0% and the 9.6%. `docs/phase-1-findings.md`
//! records all three, along with the caveat that none of them varies the three
//! measures individually. An
//! 802.11 acknowledgement comes from the receiver's MAC hardware, so its
//! absence means the radio was simply not on the channel: NimBLE and
//! Wi-Fi share the one 2.4 GHz antenna, and the admin window is precisely when
//! the node is otherwise idle and the Bluetooth controller is free to take it.
//!
//! Two things here are meant to avoid repeating that.
//!
//! The scan is **bounded and switched off**, not left running. `HCI_LE_Set_Scan_
//! Enable(0)` stops the controller taking the antenna at all, which is stronger
//! than the vendor's `while (pBLEScan->isScanning()) delay(1)`
//! (`src/WiFiOps.cpp:1452-1453`): that waits for a scan to finish while leaving
//! an initialised NimBLE stack behind it, and the measurements say something in
//! that stack keeps the radio.
//!
//! And it runs **at the far end of the sweep from the admin window** — reported
//! and finished before the heartbeat goes out, never overlapping the 300 ms the
//! node has to be listening in. That is a claim about wall-clock distance, not
//! about position in the loop, so the caller also rate-limits it: a node
//! assigned a single channel completes a sweep every 125 ms, and "once per
//! sweep" would put a 500 ms scan against nearly every admin window it has.
//! `BLE_INTERVAL_MS` in `main.rs` is what keeps the two apart.
//!
//! There is no host stack here. `esp-radio` exposes the controller as a raw HCI
//! pipe and all this firmware wants from Bluetooth is an address and a signal
//! strength, so the packets are built and read by `wartui_proto::hci` and the
//! only thing in this file is the conversation.

use esp_hal::peripherals::BT;
use esp_hal::time::{Duration, Instant};
use esp_radio::ble::Config;
use esp_radio::ble::controller::BleConnector;
use esp_rtos::CurrentThreadHandle;
use wartui_proto::hci::{
    AdvReport, PACKET_MAX, RESET, SCAN_UNIT_US, SET_EVENT_MASK, adv_reports, set_scan_enable,
    set_scan_parameters,
};

/// How long one sweep listens. `BLE_SCAN_DURATION`, `src/configs.h:72`.
pub const SCAN_MS: u32 = 500;

/// Distinct advertisers one sweep will hold.
///
/// Addresses rotate for privacy, so unlike access points these rarely repeat
/// and the ring behind them never suppresses much. Sixty-four was a guess at a
/// busy room, and then an ordinary room filled 60 of it and, on the re-run, 61
/// (`docs/phase-1-findings.md`). The ring drops the *newest* advertiser once it
/// is full and records that only on a counter no host reads, which makes
/// overflow the one failure here that nothing would notice — so the number is
/// set well clear of the measurement rather than just above it.
///
/// It is also the ceiling on something else, which is the reason not to make it
/// enormous: `report_ble` broadcasts one frame per *new* advertiser, and it runs
/// immediately before the stagger, the heartbeat and the admin window. Addresses
/// rotate, so "new" is most of a sweep — 54 of the first scan's 54. This number
/// is therefore the worst-case burst standing between the last dwell and the
/// window this node has to be listening in, which is the delay the whole module
/// is arranged to avoid.
///
/// Eighty and not more because `Scanner` is built by value, so the ring is on
/// the stack of `Scanner::new` and then of `main`: at 96 entries a `esp32c5,ble`
/// build trips `clippy::large_stack_frames`, which `main.rs` denies. The C5 is
/// the binding one — a C6 gets as far as 112 — and the margin here is deliberate
/// so a dependency bump does not land on the limit. Wanting a ring bigger than
/// this is a reason to move the reports into a `static`, the way `sniff` holds
/// its sightings, not a reason to raise the threshold.
const REPORTS: usize = 80;

/// Listen continuously while enabled: interval and window equal, at 30 ms.
const SCAN_WINDOW: u16 = (30_000 / SCAN_UNIT_US) as u16;

/// Granularity of the collection loop.
const POLL_MS: u64 = 2;

pub struct Scanner<'d> {
    connector: BleConnector<'d>,
    packet: [u8; PACKET_MAX],
    reports: [AdvReport; REPORTS],
    len: usize,
    /// Advertisers lost to a full buffer, since boot.
    dropped: u32,
}

impl<'d> Scanner<'d> {
    /// Bring the controller up and tell it how to scan. Scanning stays off.
    pub fn new(bt: BT<'d>) -> Option<Self> {
        let mut scanner = Self {
            connector: BleConnector::new(bt, Config::default()).ok()?,
            packet: [0; PACKET_MAX],
            reports: [AdvReport { address: [0; 6], rssi: 0 }; REPORTS],
            len: 0,
            dropped: 0,
        };
        // A reset first, because the controller keeps whatever state the last
        // run left it in and enabling a scan twice is an error rather than a
        // no-op.
        scanner.command(&RESET)?;
        // After the reset, because the reset restores the default mask that
        // hides advertising reports. See `SET_EVENT_MASK`.
        scanner.command(&SET_EVENT_MASK)?;
        scanner.command(&set_scan_parameters(SCAN_WINDOW, SCAN_WINDOW))?;
        Some(scanner)
    }

    /// Listen for `SCAN_MS`, then stop, and return what was heard.
    ///
    /// Distinct addresses only, keeping the strongest reading for each: the
    /// same advertiser is heard several times a second and only one of those
    /// belongs on the wire.
    ///
    /// "Heard" is per call and not quite per window. The disable at the end
    /// drains on a budget, so in a room busy enough to exhaust it a few reports
    /// from one scan are still queued when the next begins, and this one no
    /// longer drains before enabling — it counts them. Dedup absorbs the
    /// repeats, so what this costs is the precision of the `heard` figure in the
    /// console line, in exactly the busy case where it is least precise anyway.
    /// Draining first would cost whole reports instead, which is the trade the
    /// scan-enable path was changed to stop making.
    pub fn sweep(&mut self) -> &[AdvReport] {
        self.len = 0;
        // Written directly rather than through `command`, because from this
        // point onwards the queue is what the sweep is for. `command` drains
        // until two consecutive reads come back empty, and a room delivering a
        // packet every few milliseconds can hold it there for its whole
        // 32-iteration budget — discarding every advertising report that
        // arrives in it, and losing more of them the busier the room is. The
        // collect loop below absorbs the Command Complete instead, at no cost:
        // `adv_reports` yields nothing for a packet that is not an LE Meta
        // advertising report.
        if self.connector.write(&set_scan_enable(true)).is_err() {
            return &[];
        }

        let until = Instant::now() + Duration::from_millis(u64::from(SCAN_MS));
        while Instant::now() < until {
            match self.connector.next(&mut self.packet) {
                Ok(0) | Err(_) => CurrentThreadHandle::get().delay(Duration::from_millis(POLL_MS)),
                Ok(read) => {
                    for report in adv_reports(&self.packet[..read]) {
                        record(&mut self.reports, &mut self.len, &mut self.dropped, report);
                    }
                }
            }
        }

        // Unconditionally, and before anything else happens. This is the line
        // the whole module exists for.
        self.command(&set_scan_enable(false));
        &self.reports[..self.len]
    }

    /// Advertisers lost to a full buffer since boot.
    pub const fn dropped(&self) -> u32 {
        self.dropped
    }

    /// Send one command and drain whatever the controller says back.
    ///
    /// The completion event is not inspected. There is nothing useful to do
    /// with a controller that refuses `HCI_Reset`, and reading the queue is
    /// what stops a stale completion occupying it across a scan. It could not
    /// be *mistaken* for an advertising report — `adv_reports` yields nothing
    /// for a packet that is not one — so this is about the controller's queue
    /// space and not about misparsing, which is why `sweep` starts its scan
    /// without coming through here.
    ///
    /// Draining means draining. A command can be answered with both a Command
    /// Status and a Command Complete, so stopping at the first packet leaves
    /// one behind for the next `sweep` to read — which is the situation this
    /// function exists to prevent. It ends on a quiet queue, or on a budget:
    /// a controller that will not stop talking must not hold the sweep.
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

/// Keep `report` if it is new, or if it is a better reading than the one held.
fn record(
    reports: &mut [AdvReport; REPORTS],
    len: &mut usize,
    dropped: &mut u32,
    report: AdvReport,
) {
    if !report.has_rssi() {
        return;
    }
    if let Some(held) = reports[..*len].iter_mut().find(|r| r.address == report.address) {
        held.rssi = held.rssi.max(report.rssi);
        return;
    }
    if *len == REPORTS {
        *dropped = dropped.wrapping_add(1);
        return;
    }
    reports[*len] = report;
    *len += 1;
}
