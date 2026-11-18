//! A phone on the WardriveESP service, through the C6: its session, the fleet's Wi-Fi scan it
//! starts and stops, and the `WD:AP` lines the fleet's sightings become.
//! `wartui_proto::wardrive_esp::session` is the session itself, tested with `cargo test`.
//!
//! **The phone counts as the host, and holds the queue alone** (`wartui_proto::host_owner`).
//! While it holds it, `Shared::push_host` queues no link frame, since a phone reads text lines,
//! and the queue carries only the session's lines, cut with `\n` rather than `0x00`. They ride
//! the host stream down the C6 link under the same credit window a central's frames do
//! (`src/ble.rs`), one line a queue record, and never reach UART0 (`src/host.rs`). A phone's
//! command arrives as `Up::WardriveEspCommand` and never reaches `host::take_frame`, so it
//! confirms no image on trial.
//!
//! **Stream lines are held to [`BACKLOG_BYTES`].** The app stamps each access point with the
//! phone's position when its line arrives, so a deep backlog puts access points in the wrong
//! place. Past the bound the oldest stream line is evicted first. Answers go in the priority
//! ring, so a busy scan still answers a command.
//!
//! **The scan is the fleet's.** A start stops the fleet, then starts it on Wi-Fi alone. Each pumps
//! itself, so the `Stop` lands ahead of the `Assign` (delivery toward a node is in order), and
//! every node's dedup ring starts empty, so the phone hears each access point afresh. A stop, and
//! a phone that lets go while scanning, stop the fleet. The phone lets go on an unsubscribe, a
//! disconnect, a new C6 life, the setting turned off, or USB taking the queue.
//!
//! **A scan period is [`PERIOD`]**: the fleet has no single sweep, and a node's assigned beat is
//! the same 5 s. Each ends in a `WD:STATUS`.
//!
//! **The setting is kept in `nvs`** (`src/settings.rs`) and off by default. Whether the C6 admits
//! a phone rides `Down::Bonds` (`src/bonds.rs`); turning the setting off drops a linked phone
//! there, and `src/ble.rs` releases it once its `BleState` says so.

use core::cell::{Cell, RefCell};

use embassy_time::{Duration, Timer};
use wartui_proto::bus::{SightingBatch, Slot};
use wartui_proto::host_owner::Holder;
use wartui_proto::wardrive_esp::session::{Class, Ctx, Scan, Session, Sink};

use crate::{Shared, now_ms};

/// The most bytes of stream lines the queue holds for the phone.
const BACKLOG_BYTES: usize = 16 * 1024;

/// How often a scan period ends.
const PERIOD: Duration = Duration::from_secs(5);

/// The setting, and the session of the phone holding the queue.
pub struct Phone {
    /// Whether the bridge offers the service.
    enabled: Cell<bool>,
    session: RefCell<Session>,
    /// The host queue's drops when the phone attached.
    dropped_at: Cell<u32>,
}

impl Phone {
    /// No phone, the setting `enabled`.
    pub const fn new(enabled: bool) -> Self {
        Self {
            enabled: Cell::new(enabled),
            session: RefCell::new(Session::new()),
            dropped_at: Cell::new(0),
        }
    }

    /// Whether the bridge offers the service.
    pub fn enabled(&self) -> bool {
        self.enabled.get()
    }
}

/// The session's lines, into the host queue.
struct Lines<'a>(&'a Shared);

impl Sink for Lines<'_> {
    fn line(&mut self, class: Class, line: &[u8]) {
        let mut host = self.0.host.borrow_mut();
        let _ = match class {
            Class::Answer => host.push(line, true),
            Class::Stream => host.push_bulk_within(line, BACKLOG_BYTES),
        };
        drop(host);
        self.0.wake_link.signal(());
    }
}

/// Whether the phone holds the host queue.
fn holds(shared: &Shared) -> bool {
    shared.owner.get().holder() == Holder::Phone
}

/// Both host rings' drops.
fn dropped(shared: &Shared) -> u32 {
    let [priority, bulk] = shared.host.borrow().stats();
    priority.dropped.wrapping_add(bulk.dropped)
}

/// The bridge's state, as the session's replies read it.
#[inline(never)]
fn ctx(shared: &Shared) -> Ctx<'static> {
    let fleet = shared.fleet.borrow();
    let bridge = shared.pair.borrow().bridge_mac().map_or([0; 2], |mac| [mac[4], mac[5]]);
    Ctx {
        pool: fleet.pool(),
        version: env!("CARGO_PKG_VERSION"),
        bridge,
        // Truncated: the session reads milliseconds as `u32`.
        uptime_ms: now_ms() as u32,
        nodes: fleet.members().len() as u8,
        mtu: shared.ble.state().mtu,
        dropped: dropped(shared).wrapping_sub(shared.phone.dropped_at.get()),
    }
}

/// Carry out what the session asked of the scan.
fn apply(shared: &Shared, scan: Option<Scan>) {
    match scan {
        Some(Scan::Start) => {
            crate::fleet::stop(shared);
            crate::fleet::start(shared, false);
        }
        Some(Scan::Stop) => crate::fleet::stop(shared),
        None => {}
    }
}

/// The setting changed: keep it, and tell the C6 when whether it admits a phone changed.
pub fn set(shared: &Shared, on: bool) {
    shared.phone.enabled.set(on);
    crate::bonds::follow_admission(shared);
}

/// Make the phone the host: mark where its stream begins, cut the queue with `\n` and clear it,
/// then start its session.
#[inline(never)]
pub fn attach(shared: &Shared) {
    crate::ble::start_stream(shared);
    {
        let mut host = shared.host.borrow_mut();
        host.set_terminator(b'\n');
        host.clear();
    }
    shared.phone.dropped_at.set(dropped(shared));
    let ctx = ctx(shared);
    let scan = shared.phone.session.borrow_mut().on_subscribe(&ctx, &mut Lines(shared));
    apply(shared, scan);
    shared.wake_link.signal(());
}

/// Stop serving the phone: end its session, stop the scan it ran, and clear the queue.
pub fn release(shared: &Shared) {
    let scan = shared.phone.session.borrow_mut().release();
    apply(shared, scan);
    shared.host.borrow_mut().clear();
}

/// A command the phone wrote. Dropped unless the phone holds the queue.
#[inline(never)]
pub fn on_command(shared: &Shared, text: &[u8]) {
    if !holds(shared) {
        return;
    }
    let ctx = ctx(shared);
    let scan = shared.phone.session.borrow_mut().on_command(text, &ctx, &mut Lines(shared));
    apply(shared, scan);
}

/// A `Sightings` record from the node in `slot`: each access point in it to the phone, while the
/// phone holds the queue and scans and the slot is a member. A body that does not decode is
/// skipped whole.
#[inline(never)]
pub fn on_sightings(shared: &Shared, slot: Slot, body: &[u8]) {
    if !holds(shared)
        || !shared.phone.session.borrow().scanning()
        || !shared.fleet.borrow().is_member(slot)
    {
        return;
    }
    let Ok(batch) = SightingBatch::decode(body) else { return };
    // Truncated, as `uptime_ms` is.
    let ts_ms = now_ms() as u32;
    let mut session = shared.phone.session.borrow_mut();
    for (sighting, _) in batch.iter() {
        session.on_sighting(ts_ms, &sighting, &mut Lines(shared));
    }
}

/// End a scan period every [`PERIOD`] while the phone holds the queue.
pub async fn run(shared: &Shared) -> ! {
    loop {
        Timer::after(PERIOD).await;
        if holds(shared) {
            shared.phone.session.borrow_mut().on_period(&mut Lines(shared));
        }
    }
}
