use wartui_proto::bus::wire_epoch;
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
    /// only a power-cycle and a fan duty, which are the bridge's to carry out, go out from here.
    pub(super) fn on_command(&mut self, command: Command, now: Now, batch: &mut ActionBatch) {
        match command {
            Command::AssignBle { mac } => self.on_assign_ble(mac),
            Command::RememberBle { on } => self.on_remember_ble(on),
            Command::ClearRing { mac } => self.on_clear_dedup_ring(mac, now),
            Command::SetPool { pool } => self.on_set_pool(pool),
            Command::SetExcludedSsids { ssids } => self.config.excluded_ssids = ssids,
            Command::PowerCycle { slot } => batch.bulk.push(HostToBridge::PowerCycle { slot }),
            Command::SetFanDuty { duty } => {
                batch.bulk.push(HostToBridge::SetFan { duty });
                batch.bulk.push(HostToBridge::GetStatus);
            }
        }
    }

    /// Change the pool and re-cut the current members against it. [`Self::replan`] would return
    /// early, since membership is unchanged. With no members, the next membership change cuts
    /// against the new pool.
    fn on_set_pool(&mut self, pool: ChannelPool) {
        if pool == self.config.pool {
            return;
        }
        self.config.pool = pool;
        let members = self.plan_members.clone();
        self.cut(&members);
    }

    /// Move the Bluetooth scan, or take it off the fleet.
    ///
    /// Nothing is sent here. Moving the scan changes what *two* nodes scan, so it is a re-cut, and
    /// only the planner re-cuts. [`Self::replan`] reads this on every heartbeat and tick, and each
    /// end takes its new assignment on its own heartbeat, share and flag under one epoch.
    ///
    /// While remembering is on, the preferred node follows the target, `None` included, so a
    /// withdrawn scan stays withdrawn. It is set before the no-op check, so naming the current
    /// holder still records it.
    fn on_assign_ble(&mut self, target: Option<Mac>) {
        if self.remember_ble {
            self.preferred_ble = target;
        }
        if self.ble_node == target {
            return;
        }

        // Each end changes on its own heartbeat, so old and new holders can overlap for up to one
        // heartbeat. "At most one node scans Bluetooth" is about what the host asks for.
        self.ble_node = target;
    }

    /// Turn remembering the preferred Bluetooth node on or off, leaving the scan where it is.
    fn on_remember_ble(&mut self, on: bool) {
        if on == self.remember_ble {
            return;
        }
        self.remember_ble = on;
        self.preferred_ble = if on { self.ble_node } else { None };
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
    /// [`Self::on_set_pool`] is the only other re-cut.
    ///
    /// Members are the assignable nodes in MAC order, the planner's slot order, so a share depends
    /// on who is present, not on arrival order. An unchanged membership returns at once, keeping
    /// this cheap enough for every tick and off a heartbeat's critical path.
    pub(super) fn replan(&mut self, now: Now) {
        // The preferred node retakes the scan when nobody holds it and it is drivable: at startup,
        // and after aging out.
        if self.ble_node.is_none()
            && let Some(mac) = self.preferred_ble
            && self.nodes.get(&mac).is_some_and(|node| self.is_assignable(node, now))
        {
            self.ble_node = Some(mac);
        }
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

    /// Cut the pool among `members` and deal each its share. Split out so [`Self::on_set_pool`] can
    /// re-cut an unchanged membership.
    fn cut(&mut self, members: &[(Mac, Job)]) {
        let jobs: Vec<Job> = members.iter().map(|(_, job)| *job).collect();
        // `plan_for`, not `plan`: a Bluetooth slot is left out of the deal.
        let Some(plan) = plan::plan_for(self.config.pool, &jobs) else {
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
