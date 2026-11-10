//! The P4 bridge's fleet planner: which node scans what, and the records that tell it.
//!
//! [`Fleet`] is a pure state machine shaped like [`crate::split::Pair`]. Inputs mutate it, time
//! arrives as `now_ms`, and the driver pulls what to send ([`Fleet::next_send`]) and what to
//! report ([`Fleet::take_report`]) rather than being handed them. Pull, not return, because the
//! P4 answers a send inside a borrow of its downlinks, where it cannot also queue a reply. It
//! holds a fixed array of per-slot entries keyed by [`Slot`], allocates nothing and reads no
//! clock, so `cargo test` covers what the firmware runs.
//!
//! **Every id from [`Fleet::next_send`] is answered**, once, by [`Fleet::on_delivered`] or
//! [`Fleet::on_undelivered`]: that is the driver's contract. The P4's downlink answers every
//! record it takes (delivered, given up after every try, or drained when its slot goes absent)
//! and refuses at once one it cannot take, so the core keeps no timer of its own on a send. A
//! timer would forget a record still queued behind a backed-up downlink, and every heartbeat
//! after it would queue a duplicate.
//!
//! - **A member** is a slot that is present, has heartbeated within [`HEARTBEAT_TIMEOUT_MS`], and
//!   whose heartbeat speaks [`NODE_VERSION`](crate::bus::NODE_VERSION). A heartbeat counts as
//!   presence: the P4 marks a slot present after one good transfer, and a heartbeat is one, so
//!   [`Fleet::on_heartbeat`] sets the slot's bit and only [`Fleet::on_presence`] clears it. A
//!   heartbeat from another MAC than the slot's last starts the entry afresh, and an absent slot's
//!   entry is cleared. A body that does not decode is read for its MAC alone, so a node that moved
//!   to another layout leaves the plan at once rather than after the timeout.
//! - **The plan** is [`plan_for`] over the members in ascending slot order: a member's index is
//!   the number of members below it. It is cut again only when membership, the mode, the
//!   Bluetooth pick or the pool changes, so a heartbeat that changes none of them costs nothing.
//! - **Bluetooth** goes to [`pick_bluetooth`]'s slot in [`ScanMode::WifiBluetooth`], and only
//!   once two nodes are members, so a lone node keeps sniffing Wi-Fi. The pick is sticky: it
//!   moves only when its node leaves.
//! - **A want per slot**: idle, or an [`Assign`]. A share that changes takes a new epoch from
//!   [`next_epoch`], skipping the one the slot's last heartbeat reported, since a node discards an
//!   assignment carrying the epoch it holds and a P4 that rebooted under running nodes restarts
//!   its counter. A share the cut leaves unchanged keeps its epoch, so a settled fleet is sent
//!   nothing.
//! - **A changed want goes out at once**, even over a send in flight: delivery toward a node is
//!   strictly in order, so the newer record lands last, and the older send's answer is ignored.
//!   After that a slot is re-sent its want, under the same epoch, only on a heartbeat whose epoch
//!   differs from it while nothing is in flight, or on a falling heartbeat counter, a reboot,
//!   which re-sends even over a send in flight: that send may have reached the node's last life.
//! - **Idle is [`SlotMsg::Stop`]** for a slot whose heartbeat reports a non-zero epoch, and
//!   nothing for one reporting 0. That is how [`Fleet::stop`] idles the fleet and how a core that
//!   boots stopped idles nodes still scanning from the P4's last life. A start that arrives before
//!   that Stop was pulled replaces it with an Assign.
//! - **No record goes to a node that does not speak this build's version**: its layouts are not
//!   this build's, so it is never a member and never sent a Stop either.
//! - **Send ids are [`BRIDGE_ID`] or'd with a 15-bit counter**, so they never collide with the
//!   host's, which stay below it.

use serde::{Deserialize, Serialize};

use crate::bus::{Assign, Heartbeat, MAX_SLOTS, Slot, wire_epoch};
use crate::mac::Mac;
use crate::plan::{ChannelPool, ChannelSet, Job, plan_for};

/// How long a slot stays a member after its last heartbeat.
pub const HEARTBEAT_TIMEOUT_MS: u64 = 20_000;

/// The high bit every id the core sends under carries. The host's ids stay below it.
pub const BRIDGE_ID: u16 = 0x8000;

/// The low bits of a send id.
const ID_MASK: u16 = BRIDGE_ID - 1;

/// What the fleet is asked to scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ScanMode {
    /// Nothing: every node idles.
    #[default]
    Stopped,
    /// Every member sniffs its share of the pool.
    Wifi,
    /// As [`Self::Wifi`], with one member scanning Bluetooth once there are two.
    WifiBluetooth,
}

/// One member's share, as a [`Report`] lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share {
    /// Where the member sits.
    pub slot: Slot,
    /// The epoch its assignment carries.
    pub epoch: u8,
    /// It scans Bluetooth, and so holds no channels.
    pub ble: bool,
    /// Its channels.
    pub channels: ChannelSet,
}

/// What the fleet is doing, for the host to mirror.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// The mode in force.
    pub mode: ScanMode,
    /// The pool being cut.
    pub pool: ChannelPool,
    /// Every member holding an assignment, in slot order.
    pub shares: heapless::Vec<Share, MAX_SLOTS>,
    /// Channels of the pool no member can take ([`crate::plan::Plan::unreachable`]). Empty while
    /// stopped or with no members.
    pub unreachable: ChannelSet,
}

/// A record for one slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotMsg {
    /// Scan this.
    Assign(Assign),
    /// Idle: drop the assignment.
    Stop,
}

/// A record to send, and the id its outcome comes back under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToSlot {
    /// Where it goes.
    pub slot: Slot,
    /// The id [`Fleet::on_delivered`] and [`Fleet::on_undelivered`] name it by.
    pub id: u16,
    /// What it says.
    pub msg: SlotMsg,
}

/// The slot to give the Bluetooth scan among `assignable`, a mask indexed by [`Slot`] byte like
/// `Status.present`.
///
/// `None` below two slots, so a lone node keeps Wi-Fi. Otherwise `current` while its bit is set,
/// so the scan does not move as nodes come and go, and else the highest set slot.
#[must_use]
pub fn pick_bluetooth(current: Option<Slot>, assignable: u32) -> Option<Slot> {
    if assignable.count_ones() < 2 {
        return None;
    }
    if let Some(slot) = current
        && assignable & slot.bit() != 0
    {
        return Some(slot);
    }
    #[allow(clippy::cast_possible_truncation)]
    let highest = (31 - assignable.leading_zeros()) as u8;
    Slot::from_u8(highest)
}

/// Allocate the next monotonic counter, skipping one whose wire epoch matches what the node's
/// heartbeat says it holds.
///
/// Otherwise a P4's first assignment after a restart can repeat the wire epoch a node kept, and
/// the node takes delivery and silently discards it. Consecutive counters have distinct wire
/// epochs ([`wire_epoch`]'s `% 255`), so one skip suffices. `Some(0)` never matches: `wire_epoch`
/// never returns 0.
pub fn next_epoch(last: &mut u64, held: Option<u8>) -> u64 {
    *last += 1;
    if Some(wire_epoch(*last)) == held {
        *last += 1;
    }
    *last
}

/// What the core knows of one slot.
#[derive(Debug, Clone, Copy)]
struct Entry {
    /// The MAC its last heartbeat named, or `None` since it was cleared.
    mac: Option<Mac>,
    /// The last heartbeat speaks this build's version and decoded.
    drivable: bool,
    /// When the last heartbeat arrived.
    heard_ms: u64,
    /// The last decoded heartbeat's counter, or `None` before one.
    counter: Option<u32>,
    /// The epoch the last decoded heartbeat reported.
    held: u8,
    /// What it should scan, or `None` to idle.
    want: Option<Assign>,
    /// The id of the send awaiting an outcome, if any.
    in_flight: Option<u16>,
    /// The want is to go out on the next [`Fleet::next_send`].
    owed: bool,
}

impl Entry {
    const EMPTY: Self = Self {
        mac: None,
        drivable: false,
        heard_ms: 0,
        counter: None,
        held: 0,
        want: None,
        in_flight: None,
        owed: false,
    };

    /// The epoch a settled node reports: the want's, or 0 for idle.
    fn target(&self) -> u8 {
        self.want.map_or(0, |assign| assign.epoch)
    }

    /// Whether the core sends this slot anything at all.
    fn sendable(&self) -> bool {
        self.mac.is_some() && self.drivable
    }
}

/// The P4's fleet planner. See the module documentation.
#[derive(Debug, Clone)]
pub struct Fleet {
    mode: ScanMode,
    pool: ChannelPool,
    /// Bit n for slot byte n: the slot holds a node.
    present: u32,
    entries: [Entry; MAX_SLOTS],
    /// The members the current plan was cut for.
    members: u32,
    /// The slot scanning Bluetooth in the current plan.
    bluetooth: Option<Slot>,
    /// The mode or pool changed, or an entry lost its want, since the last cut.
    recut: bool,
    unreachable: ChannelSet,
    /// The epoch counter, narrowed by [`wire_epoch`].
    last_epoch: u64,
    /// The low bits of the next send id.
    next_id: u16,
    /// Something a [`Report`] carries changed since [`Fleet::take_report`].
    changed: bool,
}

impl Fleet {
    /// A stopped fleet cutting `pool`, which has heard nothing.
    #[must_use]
    pub const fn new(pool: ChannelPool) -> Self {
        Self {
            mode: ScanMode::Stopped,
            pool,
            present: 0,
            entries: [Entry::EMPTY; MAX_SLOTS],
            members: 0,
            bluetooth: None,
            recut: false,
            unreachable: ChannelSet::empty(),
            last_epoch: 0,
            next_id: 0,
            changed: false,
        }
    }

    /// Scan Wi-Fi, and Bluetooth too when `bluetooth`.
    pub fn start(&mut self, bluetooth: bool, now_ms: u64) {
        let mode = if bluetooth { ScanMode::WifiBluetooth } else { ScanMode::Wifi };
        self.set_mode(mode, now_ms);
    }

    /// Idle every node.
    pub fn stop(&mut self, now_ms: u64) {
        self.set_mode(ScanMode::Stopped, now_ms);
    }

    fn set_mode(&mut self, mode: ScanMode, now_ms: u64) {
        if self.mode != mode {
            self.mode = mode;
            self.recut = true;
            self.changed = true;
        }
        self.settle(now_ms);
    }

    /// Cut `pool` from now on.
    pub fn set_pool(&mut self, pool: ChannelPool, now_ms: u64) {
        if self.pool != pool {
            self.pool = pool;
            self.recut = true;
            self.changed = true;
        }
        self.settle(now_ms);
    }

    /// A heartbeat `body` arrived from `slot`.
    pub fn on_heartbeat(&mut self, slot: Slot, body: &[u8], now_ms: u64) {
        let (mac, beat) = match Heartbeat::decode(body) {
            Ok(beat) => (beat.mac, Some(beat)),
            Err(_) => match Heartbeat::identity(body) {
                Ok((_, mac)) => (mac, None),
                Err(_) => return,
            },
        };
        self.present |= slot.bit();
        let index = usize::from(slot.as_u8());
        if self.entries[index].mac != Some(mac) {
            self.clear(index);
            self.entries[index].mac = Some(mac);
        }
        let entry = &mut self.entries[index];
        entry.heard_ms = now_ms;
        match beat {
            Some(beat) => {
                entry.drivable = beat.capabilities.version.compat_with_host().drivable();
                let fell = entry.counter.is_some_and(|last| beat.counter < last);
                entry.counter = Some(beat.counter);
                entry.held = beat.epoch;
                if fell && entry.want.is_some() {
                    entry.owed = true;
                }
            }
            None => entry.drivable = false,
        }
        self.settle(now_ms);
        let entry = &mut self.entries[index];
        if entry.sendable() && entry.in_flight.is_none() && entry.held != entry.target() {
            entry.owed = true;
        }
    }

    /// The slots holding a node are now `present`, bit n for slot byte n.
    pub fn on_presence(&mut self, present: u32, now_ms: u64) {
        let gone = self.present & !present;
        self.present = present;
        for index in 0..MAX_SLOTS {
            if gone & (1 << index) != 0 {
                self.clear(index);
            }
        }
        self.settle(now_ms);
    }

    /// Forget what a slot held. A want it loses is a change the next cut must see.
    fn clear(&mut self, index: usize) {
        if self.entries[index].want.is_some() {
            self.recut = true;
            self.changed = true;
        }
        self.entries[index] = Entry::EMPTY;
    }

    /// Send `id` reached its node.
    ///
    /// The node is taken to hold what it was sent until its next heartbeat says otherwise.
    /// Without that, a stop arriving between an Assign's delivery and the heartbeat that shows
    /// its epoch would find a node reporting 0 and send it no Stop.
    pub fn on_delivered(&mut self, id: u16, _now_ms: u64) {
        if let Some(entry) = self.flight(id) {
            entry.in_flight = None;
            if !entry.owed {
                entry.held = entry.target();
            }
        }
    }

    /// Send `id` did not reach its node. The next heartbeat whose epoch differs from the want
    /// sends it again.
    pub fn on_undelivered(&mut self, id: u16, _now_ms: u64) {
        if let Some(entry) = self.flight(id) {
            entry.in_flight = None;
        }
    }

    /// The entry whose send in flight is `id`.
    fn flight(&mut self, id: u16) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|entry| entry.in_flight == Some(id))
    }

    /// The P4 clock reached `now_ms`: drop members silent past [`HEARTBEAT_TIMEOUT_MS`].
    pub fn on_tick(&mut self, now_ms: u64) {
        self.settle(now_ms);
    }

    /// The earliest P4 time at which [`Fleet::on_tick`] acts, or `None` while nothing waits on
    /// the clock. A caller sleeps until then instead of polling.
    #[must_use]
    pub fn next_deadline_ms(&self) -> Option<u64> {
        slots_of(self.members)
            .map(|index| self.entries[index].heard_ms + HEARTBEAT_TIMEOUT_MS)
            .min()
    }

    /// The next record to send, lowest slot first, or `None` when nothing is owed. A caller
    /// drains it after every input.
    pub fn next_send(&mut self, _now_ms: u64) -> Option<ToSlot> {
        for index in 0..MAX_SLOTS {
            let entry = &mut self.entries[index];
            if !entry.owed {
                continue;
            }
            entry.owed = false;
            if !entry.sendable() {
                continue;
            }
            let msg = match entry.want {
                Some(assign) => SlotMsg::Assign(assign),
                // An idle node needs nothing, unless an Assign may still be on its way to it.
                None if entry.held == 0 && entry.in_flight.is_none() => continue,
                None => SlotMsg::Stop,
            };
            let id = BRIDGE_ID | self.next_id;
            self.next_id = self.next_id.wrapping_add(1) & ID_MASK;
            let entry = &mut self.entries[index];
            entry.in_flight = Some(id);
            #[allow(clippy::cast_possible_truncation)]
            let slot = Slot::from_u8(index as u8)?;
            return Some(ToSlot { slot, id, msg });
        }
        None
    }

    /// The current [`Report`] if it changed since the last call, once.
    pub fn take_report(&mut self) -> Option<Report> {
        core::mem::take(&mut self.changed).then(|| self.report())
    }

    /// What the fleet is doing now.
    #[must_use]
    pub fn report(&self) -> Report {
        let mut shares = heapless::Vec::new();
        for (index, entry) in self.entries.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            if let Some(assign) = entry.want
                && let Some(slot) = Slot::from_u8(index as u8)
            {
                // One per slot, and there are `MAX_SLOTS`.
                let _ = shares.push(Share {
                    slot,
                    epoch: assign.epoch,
                    ble: assign.scan_ble(),
                    channels: assign.channels,
                });
            }
        }
        Report { mode: self.mode, pool: self.pool, shares, unreachable: self.unreachable }
    }

    /// The mode in force.
    #[must_use]
    pub const fn mode(&self) -> ScanMode {
        self.mode
    }

    /// The pool being cut.
    #[must_use]
    pub const fn pool(&self) -> ChannelPool {
        self.pool
    }

    /// The slot scanning Bluetooth, if any.
    #[must_use]
    pub const fn bluetooth(&self) -> Option<Slot> {
        self.bluetooth
    }

    /// The members at `now_ms`, bit n for slot byte n.
    fn member_mask(&self, now_ms: u64) -> u32 {
        let mut mask = 0;
        for (index, entry) in self.entries.iter().enumerate() {
            let bit = 1 << index;
            if self.present & bit != 0
                && entry.sendable()
                && now_ms.saturating_sub(entry.heard_ms) < HEARTBEAT_TIMEOUT_MS
            {
                mask |= bit;
            }
        }
        mask
    }

    /// Cut again if membership, the mode, the Bluetooth pick or the pool changed.
    fn settle(&mut self, now_ms: u64) {
        let members = self.member_mask(now_ms);
        let pick = match self.mode {
            ScanMode::WifiBluetooth => pick_bluetooth(self.bluetooth, members),
            ScanMode::Wifi | ScanMode::Stopped => None,
        };
        if !self.recut && members == self.members && pick == self.bluetooth {
            return;
        }
        self.recut = false;
        self.members = members;
        self.bluetooth = pick;
        self.cut();
    }

    /// Deal the pool to the members and give each slot its want.
    fn cut(&mut self) {
        let mut jobs = [Job::Wifi; MAX_SLOTS];
        let mut count = 0;
        for (index, job) in slots_of(self.members).zip(jobs.iter_mut()) {
            if self.bluetooth.is_some_and(|slot| usize::from(slot.as_u8()) == index) {
                *job = Job::Bluetooth;
            }
            count += 1;
        }
        let plan = match self.mode {
            ScanMode::Stopped => None,
            ScanMode::Wifi | ScanMode::WifiBluetooth => plan_for(self.pool, &jobs[..count]),
        };
        let unreachable = plan.map_or(ChannelSet::empty(), |plan| plan.unreachable());
        if unreachable != self.unreachable {
            self.unreachable = unreachable;
            self.changed = true;
        }
        let mut member = 0u8;
        for index in 0..MAX_SLOTS {
            let want = if self.members & (1 << index) == 0 {
                None
            } else {
                let ble = self.bluetooth.is_some_and(|slot| usize::from(slot.as_u8()) == index);
                member += 1;
                plan.and_then(|plan| plan.assign_for(member - 1, 0, Assign::flags_for(ble)))
            };
            self.deal(index, want);
        }
    }

    /// Give slot `index` the want `next`, whose epoch is not yet chosen.
    fn deal(&mut self, index: usize, next: Option<Assign>) {
        let entry = &mut self.entries[index];
        match (entry.want, next) {
            (None, None) => {}
            (Some(old), Some(new)) if old.flags == new.flags && old.channels == new.channels => {}
            (_, Some(mut new)) => {
                new.epoch = wire_epoch(next_epoch(&mut self.last_epoch, Some(entry.held)));
                entry.want = Some(new);
                entry.owed = entry.sendable();
                self.changed = true;
            }
            (Some(_), None) => {
                entry.want = None;
                entry.owed = entry.sendable() && (entry.held != 0 || entry.in_flight.is_some());
                self.changed = true;
            }
        }
    }
}

/// The indices of the slots in `mask`, ascending.
fn slots_of(mask: u32) -> impl Iterator<Item = usize> {
    (0..MAX_SLOTS).filter(move |index| mask & (1 << index) != 0)
}
