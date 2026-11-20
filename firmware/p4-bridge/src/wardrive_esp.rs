//! A phone on the WardriveESP service, through the C6: its session, the fleet's scan it starts
//! and stops, and the `WD:AP` and `WD:BT` lines the fleet's sightings become.
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
//! **The scan is the fleet's.** A start stops the fleet, then starts it on Wi-Fi, with Bluetooth
//! beside it in the session's Bluetooth mode. Each pumps itself, so the `Stop` lands ahead of the
//! `Assign` (delivery toward a node is in order), and every node's dedup ring starts empty, so the
//! phone hears each device afresh. Every sighting record goes to the session, which sends only
//! its mode's kind. A stop, and a phone that lets go while scanning, stop the fleet. The phone
//! lets go on an unsubscribe, a disconnect, a new C6 life, the setting turned off, or USB taking
//! the queue.
//!
//! **The session's channel list is given to the planner** after each attach, command and release,
//! and outranks the pool there. A start gives it between its stop and its start, so a start costs
//! one cut. A list changed while scanning re-cuts at once, with no stop, so no dedup ring is
//! cleared. A release clears the list, so the next host's scan deals the pool.
//!
//! **A scan period is [`PERIOD`], counted from the scan's start**: the fleet has no single sweep,
//! and a node's assigned beat is the same 5 s. Each ends in a `WD:STATUS`, so the first lands
//! [`PERIOD`] after `WD:BEGIN`. A `WD:HEALTH` goes every [`HEALTH_EVERY`] while the phone holds
//! the queue, scanning or not.
//!
//! **The C6 measures the phone's link** and sends it as `Up::PhoneLink`, just ahead of the
//! `BleState` that attaches the phone, a second after, then every 5 s, and at once when the
//! link's timing changes, so `WD:LINK` in the scan header, `WD:HEALTH` and the `link` answer read
//! the latest. A report's two ages, the connection's and the last drop's, gain the seconds since
//! it arrived, so each line reads as of when it is written. The session sends `WD:CONN` when a
//! report's timing differs from the one it last told the phone of, and the report ahead of the
//! attach is its baseline. `WD:BOOT` reads the P4's own image: its slot, its trial
//! (`src/update.rs`) and why it last reset. A phone that sends `drop` is answered first;
//! `Down::DropPhone` then asks the C6 to disconnect it, which releases it here.
//!
//! **The P4 holds what the status beacon and advert extras say** (`wardrive_esp::beacon`): the
//! session's scan, count, `iam` and channel list, or a fresh session's while no phone holds the
//! queue, and how the last phone link ended. A release ends it as `Released` when the app sent
//! `bye` first, else as `Lost`; it is `None` until a phone has held the queue since boot.
//! `Down::Beacon` goes when a session event (an attach, a release, a command or a period) leaves
//! it saying something other than the last the down queue took, after each hello answer, since a
//! C6 that has just said hello holds none, and [`BEACON_EVERY`] after the last attempt, beside the
//! bonds' repeat. The repeat covers what no session event follows, such as a pool change. A push
//! the full down queue refuses is counted with its other refusals and sent again at the repeat.
//! The C6 encodes it.
//!
//! **The setting is kept in `nvs`** (`src/settings.rs`) and off by default. Whether the C6 admits
//! a phone rides `Down::Bonds` (`src/bonds.rs`); turning the setting off drops a linked phone
//! there, and `src/ble.rs` releases it once its `BleState` says so.

use core::cell::{Cell, RefCell};

use embassy_futures::select::{Either3, select3};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use wartui_proto::bus::{SightingBatch, Slot};
use wartui_proto::host_owner::Holder;
use wartui_proto::radio_link::{Down, PhoneLink, Stream};
use wartui_proto::reset::ResetCause;
use wartui_proto::wardrive_esp::beacon::{Beacon, LinkEnd};
use wartui_proto::wardrive_esp::session::{Ask, Boot, Class, Ctx, Mode, Session, Sink};

use crate::{Shared, now_ms};

/// The most bytes of stream lines the queue holds for the phone.
const BACKLOG_BYTES: usize = 16 * 1024;

/// How often a scan period ends.
const PERIOD: Duration = Duration::from_secs(5);

/// How often the phone is sent `WD:HEALTH`.
const HEALTH_EVERY: Duration = Duration::from_secs(15);

/// How often the C6 is sent the beacon again, changed or not, in P4 milliseconds.
const BEACON_EVERY: u64 = 5_000;

/// The setting, and the session of the phone holding the queue.
pub struct Phone {
    /// Whether the bridge offers the service.
    enabled: Cell<bool>,
    session: RefCell<Session>,
    /// The host queue's drops when the phone attached.
    dropped_at: Cell<u32>,
    /// A scan started: the period starts over.
    restart: Signal<NoopRawMutex, ()>,
    /// The phone's link, as the C6 last reported it.
    link: Cell<PhoneLink>,
    /// When that report arrived.
    link_at: Cell<Instant>,
    /// Why the P4 last reset.
    reset: ResetCause,
    /// How the last phone link ended.
    link_end: Cell<LinkEnd>,
    /// The latest `Down::Beacon` the down queue took, or `None` before one.
    beacon: RefCell<Option<Beacon>>,
    /// P4 milliseconds of the last attempt to queue it, taken or refused.
    beacon_at: Cell<u64>,
}

impl Phone {
    /// No phone, the setting `enabled`, and the P4 reset for `reset`.
    pub const fn new(enabled: bool, reset: ResetCause) -> Self {
        Self {
            enabled: Cell::new(enabled),
            session: RefCell::new(Session::new()),
            dropped_at: Cell::new(0),
            restart: Signal::new(),
            link: Cell::new(PhoneLink::NONE),
            link_at: Cell::new(Instant::from_ticks(0)),
            reset,
            link_end: Cell::new(LinkEnd::None),
            beacon: RefCell::new(None),
            beacon_at: Cell::new(0),
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
        link: aged_link(shared),
        boot: Boot {
            slot: crate::update::slot(shared),
            trial: crate::update::on_trial(shared),
            reset: shared.phone.reset,
        },
    }
}

/// Carry out what the session asked of the bridge, and give the planner the session's list.
///
/// A start gives it between the stop and the start, so the start costs one cut.
fn apply(shared: &Shared, ask: Option<Ask>) {
    match ask {
        Some(Ask::Start(mode)) => {
            crate::fleet::stop(shared);
            follow_channels(shared);
            crate::fleet::start(shared, mode == Mode::Bluetooth);
            shared.phone.restart.signal(());
            return;
        }
        Some(Ask::Stop) => crate::fleet::stop(shared),
        // The answer is queued already; the C6 holds the link a moment so it reaches the phone.
        Some(Ask::Drop) => {
            let _ = shared.push_down(&Down::DropPhone);
        }
        None => {}
    }
    follow_channels(shared);
}

/// Give the planner the session's channel list, or the pool with none. An unchanged list sends
/// nothing.
fn follow_channels(shared: &Shared) {
    let only = shared.phone.session.borrow().channels();
    crate::fleet::set_channels(shared, only);
}

/// The C6's latest report of the phone's link: kept for the session's lines, and given to the
/// session while the phone holds the queue, for `WD:CONN`.
#[inline(never)]
pub fn on_link(shared: &Shared, link: PhoneLink) {
    shared.phone.link.set(link);
    shared.phone.link_at.set(Instant::now());
    if holds(shared) {
        shared.phone.session.borrow_mut().on_link(&link, &mut Lines(shared));
    }
}

/// The latest report of the phone's link, its ages moved on by the whole seconds since it
/// arrived.
fn aged_link(shared: &Shared) -> PhoneLink {
    let mut link = shared.phone.link.get();
    let age = shared.phone.link_at.get().elapsed().as_secs() as u32;
    link.connected_s = link.connected_s.saturating_add(age);
    if let Some(drop) = &mut link.last_drop {
        drop.ago_s = drop.ago_s.saturating_add(age);
    }
    link
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
        host.set_terminator(Stream::Phone.terminator());
        host.clear();
    }
    shared.phone.dropped_at.set(dropped(shared));
    let ctx = ctx(shared);
    let ask = shared.phone.session.borrow_mut().on_subscribe(&ctx, &mut Lines(shared));
    apply(shared, ask);
    shared.wake_link.signal(());
    follow_beacon(shared);
}

/// Stop serving the phone: note how its link ended, end its session, stop the scan it ran, and
/// clear the queue.
pub fn release(shared: &Shared) {
    let ask = {
        let mut session = shared.phone.session.borrow_mut();
        let end = if session.said_bye() { LinkEnd::Released } else { LinkEnd::Lost };
        shared.phone.link_end.set(end);
        session.release()
    };
    apply(shared, ask);
    shared.host.borrow_mut().clear();
    follow_beacon(shared);
}

/// A command the phone wrote. Dropped unless the phone holds the queue.
#[inline(never)]
pub fn on_command(shared: &Shared, text: &[u8]) {
    if !holds(shared) {
        return;
    }
    let ctx = ctx(shared);
    let ask = shared.phone.session.borrow_mut().on_command(text, &ctx, &mut Lines(shared));
    apply(shared, ask);
    follow_beacon(shared);
}

/// A `Sightings` record from the node in `slot`: each sighting in it to the session, while the
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

/// End a scan period every [`PERIOD`] from the scan's start, and send `WD:HEALTH` every
/// [`HEALTH_EVERY`], while the phone holds the queue.
pub async fn run(shared: &Shared) -> ! {
    let mut period = Instant::now() + PERIOD;
    let mut health = Instant::now() + HEALTH_EVERY;
    loop {
        match select3(Timer::at(period), Timer::at(health), shared.phone.restart.wait()).await {
            Either3::First(()) => {
                period = Instant::now() + PERIOD;
                if holds(shared) {
                    end_period(shared);
                }
            }
            Either3::Second(()) => {
                health = Instant::now() + HEALTH_EVERY;
                if holds(shared) {
                    send_health(shared);
                }
            }
            Either3::Third(()) => period = Instant::now() + PERIOD,
        }
    }
}

/// End the session's scan period. Out of line, as the other session calls are.
#[inline(never)]
fn end_period(shared: &Shared) {
    let ctx = ctx(shared);
    shared.phone.session.borrow_mut().on_period(&ctx, &mut Lines(shared));
    follow_beacon(shared);
}

/// Send the phone `WD:HEALTH`. Out of line, as the other session calls are.
#[inline(never)]
fn send_health(shared: &Shared) {
    let ctx = ctx(shared);
    shared.phone.session.borrow().on_health(&ctx, &mut Lines(shared));
}

/// What the beacon says now: the session of the phone holding the queue, or a fresh one's.
#[inline(never)]
fn beacon(shared: &Shared) -> Beacon {
    let pool = shared.fleet.borrow().pool();
    let link_end = shared.phone.link_end.get();
    if holds(shared) {
        shared.phone.session.borrow().beacon(pool, link_end)
    } else {
        Session::new().beacon(pool, link_end)
    }
}

/// Send the beacon when it differs from the last the down queue took.
fn follow_beacon(shared: &Shared) {
    let now = beacon(shared);
    if shared.phone.beacon.borrow().as_ref() != Some(&now) {
        push_beacon(shared, now);
    }
}

/// Send the beacon, changed or not, once [`BEACON_EVERY`] has passed since the last attempt.
pub fn repeat_beacon(shared: &Shared) {
    if now_ms().saturating_sub(shared.phone.beacon_at.get()) >= BEACON_EVERY {
        push_beacon(shared, beacon(shared));
    }
}

/// Send the beacon, changed or not: a C6 that has just said hello holds none.
pub fn send_beacon(shared: &Shared) {
    push_beacon(shared, beacon(shared));
}

/// Queue `beacon` as `Down::Beacon`, and stamp the attempt. A refused push leaves the last
/// beacon taken as it was, so the next session event still sees the difference, and the repeat
/// sends it again.
fn push_beacon(shared: &Shared, beacon: Beacon) {
    if shared.push_down(&Down::Beacon(beacon.clone())) {
        shared.phone.beacon.replace(Some(beacon));
    }
    shared.phone.beacon_at.set(now_ms());
}
