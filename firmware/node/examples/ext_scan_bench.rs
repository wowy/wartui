//! Bench: what an extended (Bluetooth 5) scan hears that the node's legacy scan
//! does not, and what it costs.
//!
//! The node scans with the legacy commands, which report only legacy advertising
//! PDUs on the 1M PHY. This cycles for ever through its modes, 60 s each:
//!
//! - `L`: legacy, 1M — the node's own scan.
//! - `X1`: extended, 1M only.
//! - `XC`: extended, 1M and Coded, which divides listening time between the two.
//!
//! The build chooses the modes: `BENCH_MODES`, a comma-separated list, defaults to
//! `L,X1,XC`. One board per mode, side by side, compares them in the same air at
//! the same time, which a single board alternating cannot do on a drive:
//!
//! ```sh
//! BENCH_MODES=X1 cargo build --release --features esp32c6,xiao-external-antenna --example ext_scan_bench
//! ```
//!
//! An unreadable value prints a `bench: boot modes_error` line and runs the default.
//!
//! Each mode runs `src/ble.rs`'s scan: 500 ms scans back to back, disabled
//! between, interval equal to window at 30 ms, so its yield compares with the
//! product's. Every window starts from `HCI_Reset`, because the controller refuses
//! one command family after the other until reset (`wartui_proto::hci`); a reset
//! between windows of one family costs nothing measurable.
//!
//! Output is one `bench:`-prefixed line per event worth keeping, each but `boot`,
//! `setup` and `control` stamped `t=` in milliseconds since boot:
//!
//! - `bench: boot ...` — the modes, and what the controller says it supports.
//! - `bench: setup ...` — each window's command statuses.
//! - `bench: window ...` — one mode's 60 s: distinct addresses, and how they were
//!   heard, from a set of that window's own.
//! - `bench: session ...` — at each cycle end, the addresses heard only by an
//!   extended mode and never in `L`, which is the number the decision hangs on.
//! - `bench: new ...` — an address's first hearing since boot, and
//!   `bench: legacy ...` its first legacy PDU after only extended ones: enough to
//!   compare two boards' address sets offline.
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

/// Slots in the session's address set: a power of two, and eight times the
/// ~1,000 addresses the drive's first 18 minutes heard, so an 80-minute drive
/// neither fills it nor probes far. Past full, a new address is counted in
/// `overflow` and otherwise ignored.
const SESSION_SLOTS: usize = 8192;

/// Slots in the window's address set, which one 60 s window fills alone.
const WINDOW_SLOTS: usize = 1024;

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
    fn parse(name: &str) -> Option<Self> {
        match name {
            "L" => Some(Self::Legacy),
            "X1" => Some(Self::Ext1M),
            "XC" => Some(Self::ExtCoded),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Legacy => "L",
            Self::Ext1M => "X1",
            Self::ExtCoded => "XC",
        }
    }

    /// This mode's bit in [`Slot::modes`].
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

/// Most modes `BENCH_MODES` may name.
const MODES_MAX: usize = 8;

/// The modes one cycle runs, in order.
#[derive(Clone, Copy)]
struct Modes {
    list: [Mode; MODES_MAX],
    len: usize,
}

impl Modes {
    /// Every mode, once: the cycle when `BENCH_MODES` is unset or unreadable.
    const ALL: Self = Self {
        list: [
            Mode::Legacy,
            Mode::Ext1M,
            Mode::ExtCoded,
            Mode::Legacy,
            Mode::Legacy,
            Mode::Legacy,
            Mode::Legacy,
            Mode::Legacy,
        ],
        len: 3,
    };

    /// A comma-separated list of mode names, or `None` if any is unknown or
    /// there are more than [`MODES_MAX`].
    fn parse(value: &str) -> Option<Self> {
        let mut modes = Self { list: [Mode::Legacy; MODES_MAX], len: 0 };
        for name in value.split(',') {
            *modes.list.get_mut(modes.len)? = Mode::parse(name.trim())?;
            modes.len += 1;
        }
        Some(modes)
    }

    /// The build's `BENCH_MODES`, or [`Self::ALL`].
    fn configured() -> Self {
        let Some(value) = option_env!("BENCH_MODES") else {
            return Self::ALL;
        };
        Self::parse(value).unwrap_or_else(|| {
            say!("bench: boot modes_error value={:?} fallback={}", value, Self::ALL);
            Self::ALL
        })
    }

    fn iter(&self) -> impl Iterator<Item = Mode> + '_ {
        self.list[..self.len].iter().copied()
    }
}

impl core::fmt::Display for Modes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (i, mode) in self.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            f.write_str(mode.name())?;
        }
        Ok(())
    }
}

/// How an address was heard, in [`Slot::kinds`]. `HEARD` is set on every
/// filing, so a slot with no kinds is empty.
const HEARD: u8 = 1 << 0;
const LEGACY: u8 = 1 << 1;
const EXT_1M: u8 = 1 << 2;
const EXT_CODED: u8 = 1 << 3;
const MFGR: u8 = 1 << 4;

#[derive(Clone, Copy)]
struct Slot {
    address: [u8; 6],
    /// Which modes heard it.
    modes: u8,
    /// How it was heard.
    kinds: u8,
}

const EMPTY: Slot = Slot { address: [0; 6], modes: 0, kinds: 0 };

/// What filing a hearing changed.
enum Filed {
    /// The address is new to the set.
    New,
    /// The set knew the address, and this is its first legacy PDU.
    FirstLegacy,
    Known,
    /// The address is new and the set is full.
    Full,
}

/// Distinct addresses: open addressing with linear probing, so a lookup stays
/// short however many a drive hears.
struct AddressSet<const N: usize> {
    slots: [Slot; N],
    len: usize,
    /// Hearings of a new address that found the set full.
    overflow: u32,
}

impl<const N: usize> AddressSet<N> {
    const fn new() -> Self {
        const { assert!(N.is_power_of_two()) };
        Self { slots: [EMPTY; N], len: 0, overflow: 0 }
    }

    fn clear(&mut self) {
        self.slots.fill(EMPTY);
        self.len = 0;
        self.overflow = 0;
    }

    /// Note one hearing of `address` by `modes`, as `kind`.
    fn file(&mut self, address: [u8; 6], modes: u8, kind: u8) -> Filed {
        let mut at = fnv1a(address) as usize & (N - 1);
        for _ in 0..N {
            let slot = &mut self.slots[at];
            if slot.kinds == 0 {
                *slot = Slot { address, modes, kinds: kind | HEARD };
                self.len += 1;
                return Filed::New;
            }
            if slot.address == address {
                let first_legacy = kind & LEGACY != 0 && slot.kinds & LEGACY == 0;
                slot.modes |= modes;
                slot.kinds |= kind | HEARD;
                return if first_legacy { Filed::FirstLegacy } else { Filed::Known };
            }
            at = (at + 1) & (N - 1);
        }
        self.overflow = self.overflow.wrapping_add(1);
        Filed::Full
    }

    fn entries(&self) -> impl Iterator<Item = &Slot> + Clone {
        self.slots.iter().filter(|s| s.kinds != 0)
    }

    /// Distinct addresses heard with every bit of `kind`.
    fn count(&self, kind: u8) -> usize {
        self.entries().filter(|s| s.kinds & kind == kind).count()
    }
}

/// FNV-1a over the address, 32-bit.
fn fnv1a(address: [u8; 6]) -> u32 {
    address
        .iter()
        .fold(0x811C_9DC5, |hash, &octet| (hash ^ u32::from(octet)).wrapping_mul(0x0100_0193))
}

/// One hearing of an addressed advertiser.
struct Hearing {
    address: [u8; 6],
    legacy: bool,
    primary_phy: u8,
    rssi: i8,
    kind: u8,
}

/// The addresses heard since boot, which feed the session line and the
/// `new`/`legacy` lines, and those heard in the window running, which feed the
/// window line. Separate so a full session cannot blind a window.
///
/// In a `static` rather than on `main`'s stack, as `src/ble.rs` holds its
/// reports, and taken once as `&'static mut` because only the main loop reads it.
struct Sets {
    session: AddressSet<SESSION_SLOTS>,
    window: AddressSet<WINDOW_SLOTS>,
}

impl Sets {
    const fn new() -> Self {
        Self { session: AddressSet::new(), window: AddressSet::new() }
    }

    fn file(&mut self, mode: Mode, hearing: &Hearing) {
        self.window.file(hearing.address, mode.bit(), hearing.kind);
        match self.session.file(hearing.address, mode.bit(), hearing.kind) {
            Filed::New => say!(
                "bench: new t={} mode={} addr={} legacy={} phy={} rssi={}",
                now_ms(),
                mode.name(),
                MacFmt(hearing.address),
                hearing.legacy,
                hearing.primary_phy,
                hearing.rssi
            ),
            Filed::FirstLegacy => say!(
                "bench: legacy t={} mode={} addr={}",
                now_ms(),
                mode.name(),
                MacFmt(hearing.address)
            ),
            Filed::Known | Filed::Full => {}
        }
    }

    fn summary(&self, cycle: u32) {
        let extended = Mode::Ext1M.bit() | Mode::ExtCoded.bit();
        let ext_only = self
            .session
            .entries()
            .filter(|e| e.modes & extended != 0 && e.modes & Mode::Legacy.bit() == 0);
        let never_legacy = ext_only.clone().filter(|e| e.kinds & LEGACY == 0);
        say!(
            "bench: session t={} cycle={} distinct={} ext_only={} ext_only_never_legacy={} \
             never_legacy_1m={} never_legacy_coded_only={} overflow={}",
            now_ms(),
            cycle,
            self.session.len,
            ext_only.count(),
            never_legacy.clone().count(),
            never_legacy.clone().filter(|e| e.kinds & EXT_1M != 0).count(),
            never_legacy.filter(|e| e.kinds & (EXT_1M | EXT_CODED) == EXT_CODED).count(),
            self.session.overflow,
        );
    }
}

static SETS: ConstStaticCell<Sets> = ConstStaticCell::new(Sets::new());

/// Milliseconds since boot, for lining two boards' logs up offline.
fn now_ms() -> u64 {
    Instant::now().duration_since_epoch().as_millis()
}

/// Report counts for one mode window; the distinct counts live in [`Sets::window`].
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

    let modes = Modes::configured();
    say!("bench: boot modes={}", modes);
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

    let sets = SETS.take();
    let mut cycle = 0u32;
    loop {
        cycle = cycle.wrapping_add(1);
        for mode in modes.iter() {
            run_mode(&mut hci, sets, mode, cycle);
        }
        sets.summary(cycle);
    }
}

/// Reset into `mode`, scan for [`MODE_MS`], and print the window.
fn run_mode(hci: &mut Hci, sets: &mut Sets, mode: Mode, cycle: u32) {
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

    sets.window.clear();
    let mut window = Window::default();
    let end = Instant::now() + Duration::from_millis(MODE_MS);
    while Instant::now() < end {
        scan(hci, sets, mode, &mut window);
    }

    say!(
        "bench: window t={} mode={} cycle={} scans={} reports={} distinct={} legacy={} ext_1m={} \
         ext_coded={} mfgr={} anonymous={} incomplete={} truncated={} overflow={} \
         window_overflow={} enable_failed={} disable_failed={}",
        now_ms(),
        mode.name(),
        cycle,
        window.scans,
        window.reports,
        sets.window.count(HEARD),
        sets.window.count(LEGACY),
        sets.window.count(EXT_1M),
        sets.window.count(EXT_CODED),
        sets.window.count(MFGR),
        window.anonymous,
        window.incomplete,
        window.truncated,
        sets.session.overflow,
        sets.window.overflow,
        window.enable_failed,
        window.disable_failed,
    );
}

/// One scan of [`SCAN_MS`], as `src/ble.rs`'s `sweep`: enabled by a bare write so
/// the queue is the scan's, then disabled unconditionally.
fn scan(hci: &mut Hci, sets: &mut Sets, mode: Mode, window: &mut Window) {
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
                file(packet, sets, mode, window);
            }
        }
    }

    // Reports still queued behind the disable belong to this scan.
    if hci.command(disable, |packet| file(packet, sets, mode, window)) != Some(0) {
        window.disable_failed += 1;
    }
}

/// File every report in `packet`, of either kind.
fn file(packet: &[u8], sets: &mut Sets, mode: Mode, window: &mut Window) {
    for report in adv_reports(packet) {
        window.reports += 1;
        let kind = LEGACY | if report.mfgr.is_some() { MFGR } else { 0 };
        let hearing = Hearing {
            address: report.address,
            legacy: true,
            primary_phy: PHY_1M,
            rssi: report.rssi,
            kind,
        };
        sets.file(mode, &hearing);
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
        let hearing = Hearing {
            address: report.address,
            legacy: report.legacy,
            primary_phy: report.primary_phy,
            rssi: report.rssi,
            kind,
        };
        sets.file(mode, &hearing);
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
