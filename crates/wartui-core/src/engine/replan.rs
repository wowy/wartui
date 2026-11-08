use wartui_proto::bus::{Slot, wire_epoch};
use wartui_proto::link::HostToBridge;
use wartui_proto::mac::Mac;
use wartui_proto::plan::{self, ChannelPool, Job, Plan};

use super::{ActionBatch, Assignment, Command, FleetEngine, Now};

/// Allocate the next monotonic counter, skipping one whose wire epoch matches what the node's
/// heartbeat says it holds.
///
/// Otherwise a fresh capture's first assignment can repeat the wire epoch a node kept from a
/// previous run, and the node takes delivery and silently discards it. Consecutive counters have distinct
/// wire epochs (`wire_epoch`'s `% 255`), so one skip suffices. `Some(0)` never matches:
/// `wire_epoch` never returns 0.
pub(super) fn next_epoch(last: &mut u64, held: Option<u8>) -> u64 {
    *last += 1;
    if Some(wire_epoch(*last)) == held {
        *last += 1;
    }
    *last
}

impl FleetEngine {
    /// Take an operator's instruction. What changes for a node is sent on its next heartbeat, so
    /// only a power-cycle, a fan duty, a pool and a Bluetooth slot, which are the bridge's to carry
    /// out or hold, go out from here.
    pub(super) fn on_command(&mut self, command: Command, now: Now, batch: &mut ActionBatch) {
        match command {
            Command::AssignBle { slot } => {
                batch.bulk.push(HostToBridge::SetBle { slot });
                batch.bulk.push(HostToBridge::GetStatus);
            }
            Command::ClearRing { mac } => self.on_clear_dedup_ring(mac, now),
            Command::SetExcludedSsids { ssids } => self.config.excluded_ssids = ssids,
            Command::PowerCycle { slot } => batch.bulk.push(HostToBridge::PowerCycle { slot }),
            Command::SetFanDuty { duty } => {
                batch.bulk.push(HostToBridge::SetFan { duty });
                batch.bulk.push(HostToBridge::GetStatus);
            }
            Command::SetPool { pool } => {
                batch.bulk.push(HostToBridge::SetPool { pool });
                batch.bulk.push(HostToBridge::GetStatus);
            }
        }
    }

    /// The pool a bridge's `Status` reports: when it differs from the one held, take it and
    /// re-cut the current members against it. [`Self::replan`] would return early, since
    /// membership is unchanged. With no members, the next membership change cuts against the new
    /// pool.
    pub(super) fn on_bridge_pool(&mut self, pool: ChannelPool) {
        if self.pool == Some(pool) {
            return;
        }
        self.pool = Some(pool);
        let members = self.plan_members.clone();
        self.cut(&members);
    }

    /// The Bluetooth slot a bridge's `Status` reports: take it. The caller's [`Self::replan`]
    /// applies it, after it has also followed the nodes that `Status` shows off the bus.
    ///
    /// Nothing is sent here. Moving the scan changes what *two* nodes scan, so it is a re-cut, and
    /// only the planner re-cuts. [`Self::replan`] derives the scanning node from the slot, and each
    /// end takes its new assignment on its own heartbeat, share and flag under one epoch. So old
    /// and new holders can overlap for up to one heartbeat: "at most one node scans Bluetooth" is
    /// about what the host asks for.
    pub(super) fn on_bridge_ble(&mut self, slot: Option<Slot>) {
        self.ble_slot = slot;
    }

    /// Mark a node, or every assignable node, as owing a cleared dedup ring. A node never heard
    /// from is ignored rather than given a row. `None` reads [`Self::is_assignable`] now: the set
    /// the planner would partition.
    fn on_clear_dedup_ring(&mut self, mac: Option<Mac>, now: Now) {
        match mac {
            Some(mac) => {
                if let Some(node) = self.nodes.get_mut(&mac) {
                    node.clear_dedup_ring = true;
                }
            }
            None => {
                let targets: Vec<Mac> = self
                    .nodes
                    .values()
                    .filter(|node| self.is_assignable(node, now))
                    .map(|node| node.mac)
                    .collect();
                for mac in targets {
                    if let Some(node) = self.nodes.get_mut(&mac) {
                        node.clear_dedup_ring = true;
                    }
                }
            }
        }
    }

    /// Re-mark a node's assignment for delivery under a new epoch.
    ///
    /// For a node that rebooted and forgot it, or one whose heartbeat already holds what is queued.
    /// [`next_epoch`] avoids the epoch the node reports holding. Leaves the Bluetooth flag alone:
    /// changing it changes the channels too, which is [`Self::replan`]'s job, and clearing it alone
    /// would leave a Bluetooth node an empty set with nothing to scan.
    pub(super) fn reissue(&mut self, mac: Mac) {
        let held = self.nodes.get(&mac).and_then(|node| node.held_epoch);
        let counter = next_epoch(&mut self.last_counter, held);
        if let Some(node) = self.nodes.get_mut(&mac)
            && let Some(desired) = node.desired.as_mut()
        {
            desired.counter = counter;
            node.dirty = true;
        }
    }

    /// Hold the fleet on a partition of the pool, re-cutting when the set of nodes changes.
    /// [`Self::on_bridge_pool`] is the only other re-cut. Membership is followed before the first
    /// `Status` too, though [`Self::cut`] cuts nothing until it brings a pool.
    ///
    /// Members are the assignable nodes in MAC order, the planner's slot order, so a share depends
    /// on who is present, not on arrival order. An unchanged membership returns at once, keeping
    /// this cheap enough for every tick and off a heartbeat's critical path.
    pub(super) fn replan(&mut self, now: Now) {
        // The scan is the slot's: whichever assignable node heartbeats from it holds the scan, so
        // a node swapped into the slot takes it, and one that leaves or turns undrivable drops it.
        self.ble_node = self
            .ble_slot
            .and_then(|slot| self.slots.get(&slot).copied())
            .filter(|mac| self.nodes.get(mac).is_some_and(|node| self.is_assignable(node, now)));
        // Read once, so a node's channels and Bluetooth flag cannot disagree.
        let scanner = self.ble_node;
        let members: Vec<(Mac, Job)> = self
            .nodes
            .values()
            .filter(|node| self.is_assignable(node, now))
            .map(|node| {
                let job = if scanner == Some(node.mac) { Job::Bluetooth } else { Job::Wifi };
                (node.mac, job)
            })
            .collect();

        if members == self.plan_members {
            return;
        }

        // A departed node's dues were computed for a fleet that no longer exists, so they are
        // dropped. What it last acknowledged stays, as the best guess at what it scans.
        let departed = std::mem::replace(&mut self.plan_members, members.clone());
        for (mac, _) in departed.iter().filter(|(mac, _)| !members.iter().any(|(m, _)| m == mac)) {
            if let Some(node) = self.nodes.get_mut(mac) {
                node.desired = None;
                node.dirty = false;
            }
        }

        self.cut(&members);
    }

    /// Cut the pool among `members` and deal each its share. Split out so
    /// [`Self::on_bridge_pool`] can re-cut an unchanged membership. Nothing while no bridge has
    /// reported a pool.
    fn cut(&mut self, members: &[(Mac, Job)]) {
        let Some(pool) = self.pool else { return };
        let jobs: Vec<Job> = members.iter().map(|(_, job)| *job).collect();
        // `plan_for`, not `plan`: a Bluetooth slot is left out of the deal.
        let Some(plan) = plan::plan_for(pool, &jobs) else {
            // Nothing to cut for, or more than `plan_for` accepts. The fleet keeps what it has.
            self.plan = None;
            return;
        };
        self.counters.replans += 1;
        self.plan = Some(plan);
        self.deal(&plan, members);
    }

    /// Give every member of `plan` its share, marking a node dirty only when its share changed.
    /// `members` is the list and order `plan` was cut against, so slots line up.
    fn deal(&mut self, plan: &Plan, members: &[(Mac, Job)]) {
        // One epoch per node that needs telling, via `next_epoch`. Held locally because the node is
        // borrowed from `self` while deciding.
        let mut counter = self.last_counter;
        for (index, (mac, job)) in members.iter().enumerate() {
            let index = u8::try_from(index).unwrap_or(u8::MAX);
            let Some(node) = self.nodes.get_mut(mac) else { continue };
            // Every sniffing slot has a share, because no pool is smaller than `MAX_NODES`. An
            // empty set is the Bluetooth node's, sent with its flag. `None` is a second Bluetooth
            // slot, which `scanner` rules out.
            let Some(channels) = plan.channels_for(index) else { continue };
            // `job` comes from `scanner`, so `channels.is_empty()` and `ble` agree.
            let ble = *job == Job::Bluetooth;

            let wanted = |a: Assignment| a.channels == channels && a.ble == ble;
            // Already holds or is queued for exactly this. Re-issuing would burn an epoch.
            if node.dirty {
                if node.desired.is_some_and(wanted) {
                    continue;
                }
            } else if node.confirmed.is_some_and(wanted) {
                // Record that the node already holds what the plan wants. Otherwise a rejoining
                // node would have no `desired`, its reboot re-issue would have nothing to send, and
                // it would sit idle, silently blind.
                node.desired = node.confirmed;
                continue;
            }

            let counter = next_epoch(&mut counter, node.held_epoch);
            node.desired = Some(Assignment { channels, ble, counter });
            node.dirty = true;
        }
        self.last_counter = counter;
    }
}
