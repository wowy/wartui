//! Bluetooth scanning, when this build was asked for it.
//!
//! Off unless `--features ble`, and off for good reason. On a stock node the
//! per-sweep BLE scan costs roughly as much airtime as every channel dwell put
//! together — sixteen sweeps against nine over the same 170 seconds — and, far
//! worse, a node with BLE on acknowledged none of the thirty-two channel
//! assignments sent to it while a node without BLE acknowledged both of its
//! two. Both figures are measured; see `docs/phase-0-findings.md`. An 802.11
//! acknowledgement comes from the receiver's MAC hardware, so its absence means
//! the radio was simply not on the channel: NimBLE and Wi-Fi share the one
//! 2.4 GHz antenna, and the admin window is precisely when the node is
//! otherwise idle and the Bluetooth controller is free to take it.
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
    AdvReport, PACKET_MAX, RESET, SCAN_UNIT_US, adv_reports, set_scan_enable, set_scan_parameters,
};

/// How long one sweep listens. `BLE_SCAN_DURATION`, `src/configs.h:72`.
pub const SCAN_MS: u32 = 500;

/// Distinct advertisers one sweep will hold.
///
/// Addresses rotate for privacy, so unlike access points these rarely repeat
/// and the ring behind them never suppresses much. Sixty-four is a busy room.
const REPORTS: usize = 64;

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
        scanner.command(&set_scan_parameters(SCAN_WINDOW, SCAN_WINDOW))?;
        Some(scanner)
    }

    /// Listen for `SCAN_MS`, then stop, and return what was heard.
    ///
    /// Distinct addresses only, keeping the strongest reading for each: the
    /// same advertiser is heard several times a second and only one of those
    /// belongs on the wire.
    pub fn sweep(&mut self) -> &[AdvReport] {
        self.len = 0;
        if self.command(&set_scan_enable(true)).is_none() {
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
    /// what stops a stale completion arriving in the middle of a scan and being
    /// mistaken for an advertising report.
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
