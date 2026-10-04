use wartui_proto::air::wire_epoch;
use wartui_proto::link::HostToBridge;
use wartui_proto::mac::Mac;
use wartui_proto::plan::{self, ChannelPool, Job, Plan, Radio, clamp_tx_power};

use super::{ActionBatch, Assignment, Command, FleetEngine, Now};

/// Allocate the next monotonic counter, skipping the one whose wire epoch
/// collides with what a node's heartbeat says it already holds — otherwise a
/// fresh capture's first assignment can carry the same wire epoch a node kept
/// from a previous run, which the node acks and silently discards. Consecutive
/// counters map to distinct wire epochs (`wire_epoch`'s `% 255`), so at most one
/// skip is ever needed. `Some(0)` never matches: `wire_epoch` never returns it.
pub(super) fn next_epoch(last: &mut u64, held: Option<u8>) -> u64 {
    *last += 1;
    if Some(wire_epoch(*last)) == held {
        *last += 1;
    }
    *last
}

impl FleetEngine {
    /// Take an operator's instruction. Nothing goes out from here except a
    /// bridge transmit power, which cannot wait for a heartbeat the way an
    /// assignment can.
    pub(super) fn on_command(&mut self, command: Command, now: Now, batch: &mut ActionBatch) {
        match command {
            Command::AssignBle { mac } => self.on_assign_ble(mac),
            Command::RememberBle { on } => self.on_remember_ble(on),
            Command::SetTxPower { nodes, bridge } => self.on_set_tx_power(nodes, bridge, batch),
            Command::ClearRing { mac } => self.on_clear_dedup_ring(mac, now),
            Command::SetPool { pool } => self.on_set_pool(pool),
        }
    }

    /// Change the pool and re-cut the fleet in force against it.
    ///
    /// Membership is unchanged, so this re-cuts `plan_members` rather than going through
    /// [`Self::replan`], which returns early on an unchanged membership. With nobody in
    /// the plan there is nothing to cut, and the next membership change cuts against the
    /// new pool.
    fn on_set_pool(&mut self, pool: ChannelPool) {
        if pool == self.config.pool {
            return;
        }
        self.config.pool = pool;
        let members = self.plan_members.clone();
        self.cut(&members);
    }

    /// Change what the fleet transmits at, without touching what it scans.
    ///
    /// Both values are clamped exactly as [`FleetEngine::new`] clamps them.
    ///
    /// - **Bridge power**: Dispatched immediately ahead of the next status poll.
    ///   Because `bulk` drops rather than blocks and a dropped `SetTxPower` has
    ///   no retry until the next poll, dispatching here saves up to `status_interval`
    ///   of running at the previous power level.
    /// - **Nodes power**: Folded into the active plan via [`Self::deal`], re-issuing
    ///   member assignments under a fresh epoch to be adopted on each node's next
    ///   heartbeat. If no plan is active, transmission is deferred until the next plan.
    /// - If a value is already held, it is a no-op: no frame is sent and no epoch is spent.
    fn on_set_tx_power(&mut self, nodes: i8, bridge: i8, batch: &mut ActionBatch) {
        self.update_bridge_tx_power(clamp_tx_power(bridge), batch);
        self.update_nodes_tx_power(clamp_tx_power(nodes));
    }

    /// Update the bridge transmit power and immediately queue a control frame if connected.
    fn update_bridge_tx_power(&mut self, power: i8, batch: &mut ActionBatch) {
        if power == self.config.bridge_tx_power {
            return;
        }

        self.config.bridge_tx_power = power;
        if self.link_up {
            batch.bulk.push(HostToBridge::SetTxPower { power });
        }
    }

    /// Update the fleet node transmit power and re-deal assignments under current plan members.
    fn update_nodes_tx_power(&mut self, power: i8) {
        if power == self.config.tx_power {
            return;
        }

        self.config.tx_power = power;
        if let Some(plan) = self.plan {
            let members = self.plan_members.clone();
            self.deal(&plan, &members);
        }
    }

    /// Move the Bluetooth scan, or take it off the fleet entirely.
    ///
    /// Nothing is sent from here, and nothing is decided either. Moving the scan
    /// changes what *two* nodes scan — the one taking it stops sniffing Wi-Fi and
    /// the one giving it up takes a share of the pool back — so it is a re-cut
    /// rather than a flag flipped on an existing assignment, and the planner is
    /// still the only author of one. [`Self::replan`] runs on every heartbeat and
    /// every tick and reads this, so each end takes its new assignment in its own
    /// window, under one epoch carrying both the share and the flag.
    ///
    /// While remembering is on, the preferred node follows the target, `None` included:
    /// a scan taken off the fleet stays off rather than going straight back to the
    /// preferred node on the next re-cut. It is set before the no-op check, so naming
    /// the node that already holds the scan still records it.
    fn on_assign_ble(&mut self, target: Option<Mac>) {
        if self.remember_ble {
            self.preferred_ble = target;
        }
        if self.ble_node == target {
            return;
        }

        // Each end's frame waits on its own node's next heartbeat, so a new holder
        // that heartbeats first holds the scan alongside the old one until that
        // one's window comes round: the overlap is bounded by a heartbeat rather
        // than excluded, and "at most one node scans Bluetooth" is about what the
        // host asks for.
        self.ble_node = target;
    }

    /// Turn remembering the preferred Bluetooth node on or off, leaving the scan
    /// itself where it is.
    fn on_remember_ble(&mut self, on: bool) {
        if on == self.remember_ble {
            return;
        }
        self.remember_ble = on;
        self.preferred_ble = if on { self.ble_node } else { None };
    }

    /// Mark a node, or every assignable node, as owing a cleared dedup ring.
    ///
    /// `Some` sets the flag only if that node is in the table at all — naming one
    /// that has never been heard from is a no-op rather than a row created for
    /// it. `None` reads [`Self::is_assignable`] at the moment the command
    /// arrives, the same set the planner would partition over right now.
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
    /// For a node that rebooted, which has forgotten what it holds, or one still
    /// addressing a bridge that is gone: either way, what it was last given is
    /// re-sent under an epoch it cannot already match. Routed through
    /// [`next_epoch`] for the same reason: the epoch it is about to be re-sent
    /// under must not be the one it already reports holding.
    ///
    /// Deliberately does not touch the Bluetooth flag. A change to the flag is
    /// always a change to the channels too, so it is always a re-cut and always
    /// [`Self::replan`]'s — and clearing the flag here without the channels beside
    /// it would leave a Bluetooth node holding an empty set with nothing to scan.
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

    /// Hold the fleet on a partition of the pool, re-cutting it when the set of
    /// nodes changes. [`Self::on_set_pool`] is the only other re-cut.
    ///
    /// Membership is every node currently heartbeating; see
    /// [`Self::is_assignable`]. Nodes are ordered by MAC, which is the order the
    /// planner slots them into, so each node's share is a function of who is
    /// present rather than of the order they turned up in.
    ///
    /// Cheap on the common path: an unchanged membership returns without touching
    /// anything, which is what keeps this off a heartbeat's critical path. It is
    /// called from every tick, so that has to stay true.
    pub(super) fn replan(&mut self, now: Now) {
        // The preferred node takes the scan back whenever nothing holds it and it is
        // drivable — at startup, and after ageing out. One map lookup on the path
        // where it applies, none otherwise.
        if self.ble_node.is_none()
            && let Some(mac) = self.preferred_ble
            && self.nodes.get(&mac).is_some_and(|node| self.is_assignable(node, now))
        {
            self.ble_node = Some(mac);
        }
        // One value, read once per member below, so a node's channels and its
        // Bluetooth flag cannot disagree.
        let scanner = self.ble_node;
        let members: Vec<(Mac, Job)> = self
            .nodes
            .values()
            // Taken rather than defaulted, so no node reaches the planner with
            // a band it did not claim.
            .filter_map(|node| {
                node.capabilities.filter(|_| self.is_assignable(node, now)).map(|capabilities| {
                    let job = if scanner == Some(node.mac) {
                        Job::Bluetooth
                    } else {
                        Job::Wifi(Radio::from(capabilities))
                    };
                    (node.mac, job)
                })
            })
            .collect();

        if members == self.plan_members {
            return;
        }

        // What a departed node was owed was computed for a fleet that no longer
        // exists, so it is dropped rather than queued against a node that has
        // stopped opening windows. What it last acknowledged stays, being still
        // the best guess at what it is scanning.
        let departed = std::mem::replace(&mut self.plan_members, members.clone());
        for (mac, _) in departed.iter().filter(|(mac, _)| !members.iter().any(|(m, _)| m == mac)) {
            if let Some(node) = self.nodes.get_mut(mac) {
                node.desired = None;
                node.dirty = false;
            }
        }

        self.cut(&members);
    }

    /// Cut the pool among `members` and deal each its share.
    ///
    /// Split out of [`Self::replan`] so [`Self::on_set_pool`] can re-cut an unchanged
    /// membership against a new pool.
    fn cut(&mut self, members: &[(Mac, Job)]) {
        let jobs: Vec<Job> = members.iter().map(|(_, job)| *job).collect();
        // Not `plan`: a share of 5 GHz cut for an ESP32-C6 is a share nobody
        // scans, which is the failure the capability token exists to prevent,
        // reached by a node that is genuinely one of ours.
        let Some(plan) = plan::plan_for(self.config.pool, &jobs) else {
            // Nothing to cut for, or more than `plan_for` accepts: there is no
            // partition to be in, and the fleet keeps whatever it already had.
            self.plan = None;
            return;
        };
        self.counters.replans += 1;
        self.plan = Some(plan);
        self.deal(&plan, members);
    }

    /// Give every member of `plan` its share, marking a node dirty only when
    /// what it is told to hold has actually changed.
    ///
    /// Split out of [`Self::cut`] so [`Self::on_set_tx_power`] can re-send the
    /// plan already in force under fresh epochs without asking the planner to
    /// re-cut anything. `members` is the same list `plan` was cut against,
    /// in the same order, so each member's slot still lines up with the plan's.
    fn deal(&mut self, plan: &Plan, members: &[(Mac, Job)]) {
        // One epoch per node that actually needs telling, through `next_epoch` so
        // it never collides with what the node's own heartbeat says it holds.
        // Held locally because the decision needs the node in hand, and `self`
        // is borrowed for it.
        let mut counter = self.last_counter;
        let pool = self.config.pool;
        for (index, (mac, job)) in members.iter().enumerate() {
            let index = u8::try_from(index).unwrap_or(u8::MAX);
            let Some(node) = self.nodes.get_mut(mac) else { continue };
            // Nothing for this node: more nodes than the pool has channels *this
            // fleet* can reach, which with the radios read out of the tokens
            // means as few as twelve nodes with no 5 GHz between them. There is
            // no frame meaning "scan nothing", so it keeps what it holds —
            // duplicating another share rather than leaving a gap — and the
            // footer's unreachable line says the fleet is short of the pool.
            //
            // Unless what it holds is nothing to scan, or reaches outside the pool.
            // Nothing to scan is the assignment of a node that *was* the Bluetooth
            // scanner and no longer is: it is not duplicating a share, it is blind
            // and still holding the antenna, and leaving it alone would mean the
            // scan could never be taken off it. Channels outside the pool are what
            // a node holds after the operator narrows the pool: keeping them would
            // scan what the operator just took out. Either way it is dealt
            // everything its own radio can reach in the pool — the surplus rule at
            // its limit, and never empty, because every pool has 2.4 GHz in it and
            // every radio tunes 2.4 GHz.
            //
            // An empty set from the plan itself is neither case: it is the
            // Bluetooth node's, and it goes out with the flag beside it.
            let held = node.desired.or(node.confirmed);
            let channels = match plan.channels_for(index) {
                Some(channels) => channels,
                None if held.is_some_and(|assignment| {
                    assignment.channels.is_empty()
                        || assignment.channels.bits() & !pool.channels().bits() != 0
                }) =>
                {
                    match job.radio() {
                        Some(radio) => pool.reachable_by(radio),
                        // Unreachable: a `Job::Bluetooth` slot is never `None` above.
                        None => continue,
                    }
                }
                // The plain surplus case: nothing changes about what it scans, so
                // it keeps `held` exactly, but `SetTxPower` still has to reach it —
                // nothing else will re-send an assignment nobody re-cut. During an
                // ordinary re-cut `held`'s power already matches, so this is a
                // no-op then, same as before this arm existed.
                None => {
                    if let Some(assignment) = held
                        && assignment.tx_power != self.config.tx_power
                    {
                        let counter = next_epoch(&mut counter, node.held_epoch);
                        node.desired = Some(Assignment {
                            tx_power: self.config.tx_power,
                            counter,
                            ..assignment
                        });
                        node.dirty = true;
                    }
                    continue;
                }
            };
            // `replan` builds `job` from `scanner`, so `channels.is_empty()` and
            // `ble` always agree.
            let ble = *job == Job::Bluetooth;

            let wanted = |a: Assignment| {
                a.channels == channels && a.ble == ble && a.tx_power == self.config.tx_power
            };
            // Already scanning exactly this, or already queued to. Re-issuing
            // either would burn an epoch to tell a node what it already knows.
            if node.dirty {
                if node.desired.is_some_and(wanted) {
                    continue;
                }
            } else if node.confirmed.is_some_and(wanted) {
                // Nothing goes out, but what the plan wants and what the node
                // holds are now the same and have to be recorded as such. A node
                // rejoining a plan it already satisfies would otherwise have
                // nothing wanted of it, so its reboot re-issue would have nothing
                // to re-issue — and a node that has forgotten its assignment
                // parks on the control channel and goes silently blind.
                node.desired = node.confirmed;
                continue;
            }

            let counter = next_epoch(&mut counter, node.held_epoch);
            node.desired =
                Some(Assignment { channels, ble, tx_power: self.config.tx_power, counter });
            node.dirty = true;
        }
        self.last_counter = counter;
    }
}
