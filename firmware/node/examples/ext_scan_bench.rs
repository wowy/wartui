//! Bench: what an extended (Bluetooth 5) scan hears that the node's legacy scan
//! does not, and what it costs.
//!
//! The node scans with the legacy commands, which report only legacy advertising
//! PDUs on the 1M PHY. This cycles for ever through three modes, 60 s each:
//!
//! - `L`: legacy, 1M — the node's own scan.
//! - `X1`: extended, 1M only.
//! - `XC`: extended, 1M and Coded, which divides listening time between the two.
//!
//! Each mode runs `src/ble.rs`'s scan: 500 ms scans back to back, disabled
//! between, interval equal to window at 30 ms, so its yield compares with the
//! product's. Every mode starts from `HCI_Reset`, because the controller refuses
//! one command family after the other until reset (`wartui_proto::hci`).
//!
//! Output is one `bench:`-prefixed line per event worth keeping:
//!
//! - `bench: boot ...` — what the controller says it supports.
//! - `bench: setup ...` — each mode's command statuses.
//! - `bench: window ...` — one mode's 60 s: distinct addresses, and how they were
//!   heard.
//! - `bench: session ...` — at each cycle end, the addresses heard only by an
//!   extended mode and never in `L`, which is the number the decision hangs on.
//! - `bench: control ...` — every hearing of the positive control's addresses
//!   (`ext_adv_beacon`).
//!
//! No Wi-Fi and no ESP-NOW: the radio is Bluetooth's alone, as it is on the node
//! holding the scan.
#![no_std]
#![no_main]

#[macro_use]
mod common;

use esp_hal::time::{Duration, Instant};
use esp_radio::ble::Config;
use static_cell::ConstStaticCell;
use wartui_proto::hci::{
    DataStatus, LE_READ_LOCAL_SUPPORTED_FEATURES, PHY_1M, PHY_CODED, READ_LOCAL_SUPPORTED_COMMANDS,
    RESET, SCAN_UNIT_US, SET_EVENT_MASK, SET_LE_EVENT_MASK, ScanPhys, adv_reports,
    command_complete, ext_adv_reports, ext_scan_commands, le_features, set_ext_scan_enable,
    set_ext_scan_parameters, set_scan_enable, set_scan_parameters,
};

use common::{BEACON_1M, BEACON_CODED, Hci, MacFmt, POLL_MS, Status, delay_ms};

esp_bootloader_esp_idf::esp_app_desc!();

extern crate alloc;

/// One scan, as `src/ble.rs`'s `SCAN_MS`.
const SCAN_MS: u64 = 500;

/// Interval and window, per PHY, as `src/ble.rs`'s `SCAN_WINDOW`.
const SCAN_WINDOW: u16 = (30_000 / SCAN_UNIT_US) as u16;

/// How long each mode runs before the next.
const MODE_MS: u64 = 60_000;

/// Distinct addresses the session remembers. Beyond this a new address is
/// counted in `overflow` and otherwise ignored.
const SESSION_MAX: usize = 1024;

/// `HCI_LE_Set_Scan_Enable` and `HCI_LE_Set_Extended_Scan_Enable`.
const LEGACY_ENABLE: u16 = 0x200C;
const EXT_ENABLE: u16 = 0x2042;

#[derive(Clone, Copy)]
enum Mode {
    Legacy,
    Ext1M,
    ExtCoded,
}

impl Mode {
    const ALL: [Self; 3] = [Self::Legacy, Self::Ext1M, Self::ExtCoded];

    const fn name(self) -> &'static str {
        match self {
            Self::Legacy => "L",
            Self::Ext1M => "X1",
            Self::ExtCoded => "XC",
        }
    }

    /// This mode's bit in [`Entry::modes`].
    const fn bit(self) -> u8 {
        match self {
            Self::Legacy => 1 << 0,
            Self::Ext1M => 1 << 1,
            Self::ExtCoded => 1 << 2,
        }
    }

    /// The PHYs of an extended mode, or `None` for the legacy one.
    const fn phys(self) -> Option<ScanPhys> {
        match self {
            Self::Legacy => None,
            Self::Ext1M => Some(ScanPhys::OneM),
            Self::ExtCoded => Some(ScanPhys::OneMAndCoded),
        }
    }
}

/// How an address was heard, in [`Entry::kinds`] for the session and
/// [`Entry::window`] for the mode window running.
const HEARD: u8 = 1 << 0;
const LEGACY: u8 = 1 << 1;
const EXT_1M: u8 = 1 << 2;
const EXT_CODED: u8 = 1 << 3;
const MFGR: u8 = 1 << 4;

#[derive(Clone, Copy)]
struct Entry {
    address: [u8; 6],
    /// Which modes heard it.
    modes: u8,
    /// How it was heard across the session.
    kinds: u8,
    /// How it was heard in the current window; cleared when one starts.
    window: u8,
}

/// Every distinct address heard since boot.
///
/// In a `static` rather than on `main`'s stack, as `src/ble.rs` holds its
/// reports, and taken once as `&'static mut` because only the main loop reads it.
struct Session {
    entries: [Entry; SESSION_MAX],
    len: usize,
    /// Reports of a new address that found the set full.
    overflow: u32,
}

impl Session {
    const fn new() -> Self {
        Self {
            entries: [Entry { address: [0; 6], modes: 0, kinds: 0, window: 0 }; SESSION_MAX],
            len: 0,
            overflow: 0,
        }
    }

    fn start_window(&mut self) {
        for entry in &mut self.entries[..self.len] {
            entry.window = 0;
        }
    }

    /// Note one hearing of `address` by `mode`, as `kind`.
    fn file(&mut self, address: [u8; 6], mode: Mode, kind: u8) {
        let at = match self.entries[..self.len].iter().position(|e| e.address == address) {
            Some(at) => at,
            None if self.len < SESSION_MAX => {
                self.entries[self.len] = Entry { address, modes: 0, kinds: 0, window: 0 };
                self.len += 1;
                self.len - 1
            }
            None => {
                self.overflow = self.overflow.wrapping_add(1);
                return;
            }
        };
        let entry = &mut self.entries[at];
        entry.modes |= mode.bit();
        entry.kinds |= kind | HEARD;
        entry.window |= kind | HEARD;
    }

    /// Distinct addresses this window heard with every bit of `kind`.
    fn window_count(&self, kind: u8) -> usize {
        self.entries[..self.len].iter().filter(|e| e.window & kind == kind).count()
    }

    fn summary(&self, cycle: u32) {
        let extended = Mode::Ext1M.bit() | Mode::ExtCoded.bit();
        let ext_only = self.entries[..self.len]
            .iter()
            .filter(|e| e.modes & extended != 0 && e.modes & Mode::Legacy.bit() == 0);
        let never_legacy = ext_only.clone().filter(|e| e.kinds & LEGACY == 0);
        say!(
            "bench: session cycle={} distinct={} ext_only={} ext_only_never_legacy={} \
             never_legacy_1m={} never_legacy_coded_only={} overflow={}",
            cycle,
            self.len,
            ext_only.count(),
            never_legacy.clone().count(),
            never_legacy.clone().filter(|e| e.kinds & EXT_1M != 0).count(),
            never_legacy.filter(|e| e.kinds & (EXT_1M | EXT_CODED) == EXT_CODED).count(),
            self.overflow,
        );
    }
}

static SESSION: ConstStaticCell<Session> = ConstStaticCell::new(Session::new());

/// Report counts for one mode window; the distinct counts live in [`Session`].
#[derive(Default)]
struct Window {
    scans: u32,
    reports: u32,
    anonymous: u32,
    incomplete: u32,
    truncated: u32,
    enable_failed: u32,
    disable_failed: u32,
}

#[esp_hal::main]
fn main() -> ! {
    let bt = common::boot();
    let Some(mut hci) = Hci::new(bt, Config::default()) else {
        say!("bench: bluetooth controller would not start");
        loop {
            delay_ms(1_000);
        }
    };

    say!("bench: boot reset={}", Status(hci.command(&RESET, |_| {})));
    say!("bench: boot event_mask={}", Status(hci.command(&SET_EVENT_MASK, |_| {})));

    let mut commands = None;
    let status = hci.command(&READ_LOCAL_SUPPORTED_COMMANDS, |p| {
        commands = commands.or(ext_scan_commands(p));
    });
    match commands {
        Some(c) => say!(
            "bench: boot supported_commands={} ext_scan_parameters={} ext_scan_enable={}",
            Status(status),
            c.parameters,
            c.enable
        ),
        None => say!("bench: boot supported_commands={} unreadable", Status(status)),
    }

    let mut features = None;
    let status = hci.command(&LE_READ_LOCAL_SUPPORTED_FEATURES, |p| {
        features = features.or(le_features(p));
    });
    match features {
        Some(f) => say!(
            "bench: boot le_features={} bits={:#018x} extended_advertising={} coded_phy={}",
            Status(status),
            f.0,
            f.extended_advertising(),
            f.coded_phy()
        ),
        None => say!("bench: boot le_features={} unreadable", Status(status)),
    }

    let session = SESSION.take();
    let mut cycle = 0u32;
    loop {
        cycle = cycle.wrapping_add(1);
        for mode in Mode::ALL {
            run_mode(&mut hci, session, mode, cycle);
        }
        session.summary(cycle);
    }
}

/// Reset into `mode`, scan for [`MODE_MS`], and print the window.
fn run_mode(hci: &mut Hci, session: &mut Session, mode: Mode, cycle: u32) {
    let reset = hci.command(&RESET, |_| {});
    let mask = hci.command(&SET_EVENT_MASK, |_| {});
    match mode.phys() {
        None => {
            let params = hci.command(&set_scan_parameters(SCAN_WINDOW, SCAN_WINDOW), |_| {});
            say!(
                "bench: setup mode={} cycle={} reset={} event_mask={} scan_parameters={}",
                mode.name(),
                cycle,
                Status(reset),
                Status(mask),
                Status(params)
            );
        }
        Some(phys) => {
            let le_mask = hci.command(&SET_LE_EVENT_MASK, |_| {});
            let params = hci.command(
                set_ext_scan_parameters(phys, SCAN_WINDOW, SCAN_WINDOW).as_bytes(),
                |_| {},
            );
            say!(
                "bench: setup mode={} cycle={} reset={} event_mask={} le_event_mask={} \
                 scan_parameters={}",
                mode.name(),
                cycle,
                Status(reset),
                Status(mask),
                Status(le_mask),
                Status(params)
            );
        }
    }

    session.start_window();
    let mut window = Window::default();
    let end = Instant::now() + Duration::from_millis(MODE_MS);
    while Instant::now() < end {
        scan(hci, session, mode, &mut window);
    }

    say!(
        "bench: window mode={} cycle={} scans={} reports={} distinct={} legacy={} ext_1m={} \
         ext_coded={} mfgr={} anonymous={} incomplete={} truncated={} overflow={} \
         enable_failed={} disable_failed={}",
        mode.name(),
        cycle,
        window.scans,
        window.reports,
        session.window_count(HEARD),
        session.window_count(LEGACY),
        session.window_count(EXT_1M),
        session.window_count(EXT_CODED),
        session.window_count(MFGR),
        window.anonymous,
        window.incomplete,
        window.truncated,
        session.overflow,
        window.enable_failed,
        window.disable_failed,
    );
}

/// One scan of [`SCAN_MS`], as `src/ble.rs`'s `sweep`: enabled by a bare write so
/// the queue is the scan's, then disabled unconditionally.
fn scan(hci: &mut Hci, session: &mut Session, mode: Mode, window: &mut Window) {
    window.scans += 1;
    let ext = [set_ext_scan_enable(true), set_ext_scan_enable(false)];
    let legacy = [set_scan_enable(true), set_scan_enable(false)];
    let (enable, disable, opcode): (&[u8], &[u8], u16) = if mode.phys().is_some() {
        (&ext[0], &ext[1], EXT_ENABLE)
    } else {
        (&legacy[0], &legacy[1], LEGACY_ENABLE)
    };

    if !hci.write(enable) {
        window.enable_failed += 1;
        delay_ms(SCAN_MS);
        return;
    }
    let until = Instant::now() + Duration::from_millis(SCAN_MS);
    while Instant::now() < until {
        match hci.next() {
            None => delay_ms(POLL_MS),
            Some(packet) => {
                // The enable's own answer arrives here, among the reports.
                if let Some((op, status)) = command_complete(packet)
                    && op == opcode
                    && status != 0
                {
                    window.enable_failed += 1;
                }
                file(packet, session, mode, window);
            }
        }
    }

    // Reports still queued behind the disable belong to this scan.
    if hci.command(disable, |packet| file(packet, session, mode, window)) != Some(0) {
        window.disable_failed += 1;
    }
}

/// File every report in `packet`, of either kind.
fn file(packet: &[u8], session: &mut Session, mode: Mode, window: &mut Window) {
    for report in adv_reports(packet) {
        window.reports += 1;
        let kind = LEGACY | if report.mfgr.is_some() { MFGR } else { 0 };
        session.file(report.address, mode, kind);
        control(report.address, mode, true, PHY_1M, 0, report.rssi, report.mfgr);
    }
    for report in ext_adv_reports(packet) {
        window.reports += 1;
        match report.data_status {
            DataStatus::Complete => {}
            DataStatus::Truncated => window.truncated += 1,
            DataStatus::MoreToCome | DataStatus::Reserved => window.incomplete += 1,
        }
        if report.is_anonymous() {
            window.anonymous += 1;
            continue;
        }
        let heard_as = match (report.legacy, report.primary_phy) {
            (true, _) => LEGACY,
            (false, PHY_1M) => EXT_1M,
            (false, PHY_CODED) => EXT_CODED,
            (false, _) => 0,
        };
        let kind = heard_as | if report.mfgr.is_some() { MFGR } else { 0 };
        session.file(report.address, mode, kind);
        control(
            report.address,
            mode,
            report.legacy,
            report.primary_phy,
            report.secondary_phy,
            report.rssi,
            report.mfgr,
        );
    }
}

/// Print a hearing of the positive control, which `L` should never produce.
fn control(
    address: [u8; 6],
    mode: Mode,
    legacy: bool,
    primary: u8,
    secondary: u8,
    rssi: i8,
    mfgr: Option<u16>,
) {
    if address != BEACON_1M && address != BEACON_CODED {
        return;
    }
    say!(
        "bench: control {} mode={} legacy={} phy={}/{} rssi={} mfgr={:?}",
        MacFmt(address),
        mode.name(),
        legacy,
        primary,
        secondary,
        rssi,
        mfgr
    );
}
