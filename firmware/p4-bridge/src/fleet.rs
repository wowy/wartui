//! The P4's fleet planner: `wartui_proto::fleet::Fleet`, fed from the buses and the host, with
//! what it owes sent to the nodes and reported to the host.
//!
//! **The core is fed, then pumped.** Every input (a heartbeat, a presence change, a delivery
//! outcome, a host command, a tick) is followed by [`pump`]: every record `Fleet::next_send`
//! owes goes into its slot's `Downlink`, and a changed plan goes to the host as a priority
//! `Fleet` frame. A `StopScan` pumped only after the `StartScan` behind it would see its `Stop`
//! replaced by the new `Assign`, and the nodes would keep their dedup rings across the restart.
//!
//! **An input inside a `downlinks` borrow is recorded and deferred.** The bus answers delivery
//! outcomes while it holds its downlinks mutably, where a pump, which pushes into them, cannot
//! run. There the outcome goes to the core alone ([`answer`]) and [`Shared::fleet_wake`] wakes
//! [`run`], which pumps once the borrow is released. The core's inputs never touch the downlinks,
//! which is why it is pull-based.
//!
//! **Ids split by [`BRIDGE_ID`]**: an outcome for an id with it set is the core's, and the host,
//! whose ids stay below it, never hears of it. Every other outcome goes to the host.
//!
//! **A stop drains no downlink.** Its `Stop` queues behind whatever the slot holds. Draining the
//! slot would discard an update's queued records with the planner's, and it buys nothing:
//! delivery toward a node is strictly in order, so the `Stop` lands after any `Assign` ahead of
//! it.
//!
//! A record that does not reach a downlink, a full queue or a slot not driven, is the core's
//! `on_undelivered` at once, so it retries on a later heartbeat. The core is never borrowed across
//! an `.await`, nor while another borrow of it may be live: each call takes it and lets go.

use embassy_futures::select::select;
use embassy_time::{Instant, Timer};
use wartui_proto::bus::{DownType, Slot, Stop};
use wartui_proto::fleet::{BRIDGE_ID, Report, SlotMsg, ToSlot};
use wartui_proto::link::{BridgeToHost, Undelivered};

use crate::{Shared, now_ms};

/// How a send to a node ended.
#[derive(Clone, Copy)]
pub enum Outcome {
    /// The node acknowledged the transfer that carried it.
    Delivered,
    /// It did not reach the node.
    Undelivered(Undelivered),
}

/// Sleep until the core's next deadline, or until an input moves it, then tick and pump.
pub async fn run(shared: &Shared) -> ! {
    loop {
        let deadline = shared.fleet.borrow().next_deadline_ms();
        match deadline {
            Some(ms) => {
                select(Timer::at(Instant::from_millis(ms)), shared.fleet_wake.wait()).await;
            }
            None => shared.fleet_wake.wait().await,
        }
        shared.fleet.borrow_mut().on_tick(now_ms());
        pump(shared);
    }
}

/// Send what the core owes, then report a changed plan, and wake [`run`], since the input before
/// may have moved the core's deadline. Call after every input outside a `downlinks` borrow.
pub fn after_input(shared: &Shared) {
    pump(shared);
    shared.fleet_wake.signal(());
}

/// Push every record the core owes into its slot's downlink, then the report if it changed.
fn pump(shared: &Shared) {
    loop {
        let next = shared.fleet.borrow_mut().next_send(now_ms());
        let Some(to) = next else { break };
        if !send(shared, to) {
            shared.fleet.borrow_mut().on_undelivered(to.id, now_ms());
        }
    }
    let report = shared.fleet.borrow_mut().take_report();
    if let Some(report) = report {
        push_report(shared, &report);
    }
}

/// Queue `to` in its slot's downlink, returning whether it was queued.
#[inline(never)]
fn send(shared: &Shared, to: ToSlot) -> bool {
    let assign;
    let (kind, body): (DownType, &[u8]) = match to.msg {
        SlotMsg::Assign(msg) => {
            assign = msg.encode();
            (DownType::Assign, &assign)
        }
        SlotMsg::Stop => (DownType::Stop, &Stop.encode()),
    };
    crate::bus::push_down(shared, to.slot, to.id, kind.as_u8(), body).is_ok()
}

/// Queue `report` for the host as a `Fleet` frame. Out of line: a report is a few hundred bytes.
#[inline(never)]
fn push_report(shared: &Shared, report: &Report) {
    shared.push_host(&BridgeToHost::Fleet(report.clone()));
}

/// Queue the current plan for the host, as a host attaching or a `Status` needs.
#[inline(never)]
pub fn say(shared: &Shared) {
    let report = shared.fleet.borrow().report();
    push_report(shared, &report);
}

/// Give a heartbeat `body` from `slot` to the core, then pump.
#[inline(never)]
pub fn on_heartbeat(shared: &Shared, slot: Slot, body: &[u8]) {
    shared.fleet.borrow_mut().on_heartbeat(slot, body, now_ms());
    after_input(shared);
}

/// The slots holding a node are now `present`: tell the core, then pump.
pub fn on_presence(shared: &Shared, present: u32) {
    shared.fleet.borrow_mut().on_presence(present, now_ms());
    after_input(shared);
}

/// Route the outcome of send `id`: the core's to the core, the host's to the host.
///
/// Safe inside a `downlinks` borrow: it records the core's outcome without pumping, and wakes
/// [`run`] to pump once the borrow is released.
pub fn answer(shared: &Shared, id: u16, outcome: Outcome) {
    if id & BRIDGE_ID == 0 {
        let msg = match outcome {
            Outcome::Delivered => BridgeToHost::Delivered { id },
            Outcome::Undelivered(reason) => BridgeToHost::Undelivered { id, reason },
        };
        shared.push_host(&msg);
        return;
    }
    {
        let mut fleet = shared.fleet.borrow_mut();
        match outcome {
            Outcome::Delivered => fleet.on_delivered(id, now_ms()),
            Outcome::Undelivered(_) => fleet.on_undelivered(id, now_ms()),
        }
    }
    shared.fleet_wake.signal(());
}

/// A `StartScan`: scan Wi-Fi, and Bluetooth too when `bluetooth`, then pump.
pub fn start(shared: &Shared, bluetooth: bool) {
    shared.fleet.borrow_mut().start(bluetooth, now_ms());
    after_input(shared);
}

/// A `StopScan`: idle every node, then pump.
pub fn stop(shared: &Shared) {
    shared.fleet.borrow_mut().stop(now_ms());
    after_input(shared);
}

/// A `SetPool` the P4 runs from now on: cut `pool`, then pump.
pub fn set_pool(shared: &Shared, pool: wartui_proto::ChannelPool) {
    shared.fleet.borrow_mut().set_pool(pool, now_ms());
    after_input(shared);
}
