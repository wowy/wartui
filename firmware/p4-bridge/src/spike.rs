//! The Bluetooth host spike's load generator and counters (`wartui_proto::spike`).
//!
//! A host's `SpikeLoad` picks the profile. The generator sends only while a host owns the queue,
//! and its seq restarts at 0 whenever one attaches (`Shared::host_attached`). Records go through
//! `Shared::push_host` to the bulk ring, as a node's do.
//!
//! **It never holds the task.** The rate profiles sleep to each record's due time on the timer,
//! and drop a backlog past [`CATCH_UP`] rather than send it at once. Saturate tops the bulk ring up
//! to [`SATURATE_HEADROOM`] short of full, at most [`SATURATE_BATCH`] records a pass, every
//! [`SATURATE_POLL`], so it never evicts and the node buses keep their turns.
//!
//! `SpikeStats` goes out every `STATS_EVERY_MS` while any host owns the queue, as a priority
//! frame, so a UART0 host reads the C6's counters after its Bluetooth path stalls.
//!
//! **A `sink-discard` build needs no host to start it.** The sink's subscription starts Saturate
//! when no profile runs, and losing the queue stops it again, so a UART0 host that takes the
//! queue back reads the counters rather than the load. They freeze while UART0 owns the queue,
//! and `sent_bytes / ble_owned_ms` is the link's rate.

use core::cell::Cell;

use embassy_futures::join::join;
use embassy_futures::select::{Either, select};
use embassy_futures::yield_now;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use wartui_proto::link::{BridgeToHost, MAX_FRAME, SpikeBody};
use wartui_proto::spike::{STATS_EVERY_MS, SpikeProfile, SpikeStats, fill_body};

use crate::Shared;

/// How often an idle generator looks for a host.
const IDLE_POLL: Duration = Duration::from_millis(100);
/// How often Saturate tops the bulk ring up.
const SATURATE_POLL: Duration = Duration::from_millis(1);
/// The most records Saturate queues in one pass.
const SATURATE_BATCH: usize = 4;
/// Bulk ring bytes Saturate leaves free: two of the largest frames with their length prefixes,
/// so its own push never evicts.
const SATURATE_HEADROOM: usize = 2 * (MAX_FRAME + 2);
/// How far behind a rate profile falls before it starts its pace over.
const CATCH_UP: Duration = Duration::from_millis(50);

/// The generator's settings and the spike's own counters.
pub struct Spike {
    /// The profile a host asked for, and a burst's count.
    load: Cell<(SpikeProfile, u32)>,
    /// Raised by a `SpikeLoad`.
    changed: Signal<NoopRawMutex, ()>,
    /// The next record's seq.
    seq: Cell<u32>,
    /// Encoded frame bytes queued for the host, wrapping.
    queued: Cell<u32>,
    /// Frames the host rings refused.
    refused: Cell<u32>,
    /// The profile runs because the discard sink took the queue, not because a host asked.
    auto: Cell<bool>,
}

impl Spike {
    pub const fn new() -> Self {
        Self {
            load: Cell::new((SpikeProfile::Off, 0)),
            changed: Signal::new(),
            seq: Cell::new(0),
            queued: Cell::new(0),
            refused: Cell::new(0),
            auto: Cell::new(false),
        }
    }

    /// A host attached: its records count from 0.
    pub fn attached(&self) {
        self.seq.set(0);
    }

    /// A frame of `len` bytes was offered to the host queue, which took it or not.
    pub fn note_queued(&self, len: usize, taken: bool) {
        if taken {
            self.queued.set(self.queued.get().wrapping_add(len as u32));
        } else {
            self.refused.set(self.refused.get().wrapping_add(1));
        }
    }
}

/// A host asked for `profile`.
pub fn take(shared: &Shared, profile: SpikeProfile, count: u32) {
    shared.spike.auto.set(false);
    shared.spike.load.set((profile, count));
    shared.spike.changed.signal(());
}

/// Bluetooth took the queue or lost it. In a `sink-discard` build, start Saturate when nothing
/// runs, and stop it once the sink loses the queue.
pub fn on_bluetooth(shared: &Shared, owns: bool) {
    if !cfg!(feature = "sink-discard") {
        return;
    }
    let spike = &shared.spike;
    if owns && spike.load.get().0 == SpikeProfile::Off {
        spike.auto.set(true);
        spike.load.set((SpikeProfile::Saturate, 0));
        spike.changed.signal(());
    } else if !owns && spike.auto.replace(false) {
        spike.load.set((SpikeProfile::Off, 0));
        spike.changed.signal(());
    }
}

/// Run the generator and the counters forever.
pub async fn run(shared: &Shared) -> ! {
    join(generate(shared), report(shared)).await.0
}

/// Where the generator stands in its profile.
struct Pace {
    profile: SpikeProfile,
    /// Burst records still to send.
    burst_left: u32,
    /// When the pace started, and the records sent since.
    start: Instant,
    sent: u64,
}

impl Pace {
    /// Take the profile the host last asked for.
    fn load(&mut self, shared: &Shared) {
        (self.profile, self.burst_left) = shared.spike.load.get();
        self.restart();
    }

    fn restart(&mut self) {
        self.start = Instant::now();
        self.sent = 0;
    }

    /// The profile in force: a burst that has run out is Fleet20.
    fn current(&self) -> SpikeProfile {
        match self.profile {
            SpikeProfile::Burst if self.burst_left == 0 => SpikeProfile::Fleet20,
            profile => profile,
        }
    }
}

async fn generate(shared: &Shared) -> ! {
    let changed = &shared.spike.changed;
    let mut pace =
        Pace { profile: SpikeProfile::Off, burst_left: 0, start: Instant::now(), sent: 0 };
    loop {
        if changed.try_take().is_some() {
            pace.load(shared);
        }
        let profile = pace.current();
        if profile == SpikeProfile::Off {
            changed.wait().await;
            pace.load(shared);
            continue;
        }
        if !shared.host_open() {
            if let Either::First(()) = select(changed.wait(), Timer::after(IDLE_POLL)).await {
                pace.load(shared);
            }
            pace.restart();
            continue;
        }
        let Some(per_10s) = profile.per_10s() else {
            // Saturate.
            for _ in 0..SATURATE_BATCH {
                if shared.host.borrow().bulk_room() < SATURATE_HEADROOM {
                    break;
                }
                push_record(shared, profile.body_len());
            }
            if let Either::First(()) = select(changed.wait(), Timer::after(SATURATE_POLL)).await {
                pace.load(shared);
            }
            continue;
        };
        let due = pace.start + Duration::from_micros(pace.sent * 10_000_000 / per_10s);
        let now = Instant::now();
        if now < due {
            if let Either::First(()) = select(changed.wait(), Timer::at(due)).await {
                pace.load(shared);
            }
            continue;
        }
        if now - due > CATCH_UP {
            pace.restart();
        }
        push_record(shared, profile.body_len());
        pace.sent += 1;
        if profile == SpikeProfile::Burst {
            pace.burst_left -= 1;
            if pace.burst_left == 0 {
                pace.restart();
            }
        }
        yield_now().await;
    }
}

/// Queue the next record, with a body of `len` bytes.
#[inline(never)]
fn push_record(shared: &Shared, len: usize) {
    let seq = shared.spike.seq.get();
    shared.spike.seq.set(seq.wrapping_add(1));
    let mut body = SpikeBody::new();
    // `len` is at most a full body.
    let _ = body.resize(len, 0);
    fill_body(seq, &mut body);
    shared.push_host(&BridgeToHost::SpikeLoad { seq, body });
}

async fn report(shared: &Shared) -> ! {
    loop {
        Timer::after(Duration::from_millis(STATS_EVERY_MS)).await;
        if shared.host_open() {
            push_stats(shared);
        }
    }
}

#[inline(never)]
fn push_stats(shared: &Shared) {
    let [priority, bulk] = shared.host.borrow().stats();
    let spike = &shared.spike;
    let refused = spike.refused.get();
    let dropped = priority.dropped.saturating_add(bulk.dropped);
    let state = shared.ble.state();
    let (sent_bytes, credited_bytes) = shared.ble.totals();
    let stats = SpikeStats {
        seq: spike.seq.get(),
        uptime_ms: Instant::now().as_millis() as u32,
        ble_owned_ms: shared.ble.owned_ms() as u32,
        queued_bytes: spike.queued.get(),
        sent_bytes,
        credited_bytes,
        ble: shared.ble.stats(),
        ble_stats_age_ms: shared.ble.stats_age_ms(),
        bulk_high_water: bulk.high_water,
        refused,
        // The rings count a refusal as a drop too.
        evicted: dropped.saturating_sub(refused),
        mtu: state.mtu,
        phy: state.phy,
        interval_us: state.interval_us,
        radio_link: shared.radio_link.get(),
    };
    shared.push_host(&BridgeToHost::SpikeStats(stats));
}
