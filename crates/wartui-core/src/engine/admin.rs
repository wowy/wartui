use std::time::Instant;

use wartui_proto::bus::{Assign, Clear, DownType, Slot, wire_epoch};
use wartui_proto::link::{HostToBridge, ToNodeBody, Undelivered};
use wartui_proto::mac::Mac;

use super::{ActionBatch, Assignment, FleetEngine, Now};
use crate::record::{AdminOutcome, AssignmentSent, Record};

/// One assignment or clear in flight.
#[derive(Debug, Clone, Copy)]
pub(super) enum Pending {
    Admin(PendingAdmin),
    Clear { mac: Mac, sent_mono: Instant },
}

impl Pending {
    /// When this was sent, whichever variant it is.
    fn sent_mono(&self) -> Instant {
        match self {
            Self::Admin(pending) => pending.sent_mono,
            Self::Clear { sent_mono, .. } => *sent_mono,
        }
    }
}

/// One assignment in flight.
#[derive(Debug, Clone, Copy)]
pub(super) struct PendingAdmin {
    mac: Mac,
    assignment: Assignment,
    sent_mono: Instant,
    sent_ms: i64,
}

impl FleetEngine {
    /// Send a dirty node its assignment. Called only straight off a heartbeat, so it goes to the
    /// slot that heartbeat came from.
    ///
    /// A heartbeat read behind the bridge sends nothing.
    ///
    /// One assignment per node is in flight at a time: while one is unanswered and inside
    /// [`EngineConfig::admin_timeout`](super::EngineConfig::admin_timeout), a heartbeat sends
    /// nothing and counts nothing.
    pub(super) fn send_admin(&mut self, mac: Mac, now: Now, batch: &mut ActionBatch) {
        // A heartbeat read behind names the slot the node was in then, which may be another
        // node's now, and its epoch is history `deal` cannot skip. The next live heartbeat sends.
        if !self.reading_live() {
            return;
        }
        if self.in_flight(now, |p| matches!(p, Pending::Admin(a) if a.mac == mac)) {
            return;
        }

        let Some(node) = self.nodes.get_mut(&mac) else { return };
        if !node.dirty {
            return;
        }
        let Some(assignment) = node.desired else { return };
        let Some(slot) = node.slot else { return };
        node.admin_attempts += 1;

        let id = self.next_send_id();
        let msg = Assign {
            epoch: wire_epoch(assignment.counter),
            flags: Assign::flags_for(assignment.ble),
            channels: assignment.channels,
        };
        self.pending.insert(
            id,
            Pending::Admin(PendingAdmin {
                mac,
                assignment,
                sent_mono: now.mono,
                sent_ms: now.unix_ms,
            }),
        );
        self.counters.admin_sent += 1;
        batch.urgent.push(to_node(slot, id, DownType::Assign, &msg.encode()));
    }

    /// Send a clear to a node that owes one. Called only off a heartbeat, as
    /// [`Self::send_admin`] is. One clear per node is in flight at a time: while one is
    /// unanswered and inside the admin timeout, a heartbeat sends none.
    pub(super) fn send_dedup_ring_clear(&mut self, mac: Mac, now: Now, batch: &mut ActionBatch) {
        // A heartbeat read behind names the slot the node was in then, which may be another
        // node's now. The next live heartbeat sends.
        if !self.reading_live() {
            return;
        }
        if self.in_flight(now, |p| matches!(p, Pending::Clear { mac: m, .. } if *m == mac)) {
            return;
        }
        let Some(node) = self.nodes.get(&mac) else { return };
        let Some(slot) = node.slot else { return };
        if !node.clear_dedup_ring {
            return;
        }

        let id = self.next_send_id();
        self.pending.insert(id, Pending::Clear { mac, sent_mono: now.mono });
        batch.urgent.push(to_node(slot, id, DownType::Clear, &Clear.encode()));
    }

    /// Whether a pending send that `which` picks is in flight at `now`: unanswered, and not past
    /// [`EngineConfig::admin_timeout`](super::EngineConfig::admin_timeout).
    fn in_flight(&self, now: Now, which: impl Fn(&Pending) -> bool) -> bool {
        let timeout = self.config.admin_timeout;
        self.pending.values().any(|p| which(p) && now.mono.duration_since(p.sent_mono()) < timeout)
    }

    fn next_send_id(&mut self) -> u16 {
        let id = self.next_send_id;
        self.next_send_id = self.next_send_id.wrapping_add(1).max(1);
        id
    }

    /// The bridge delivered, or could not deliver, an assignment or a clear. `undelivered` is
    /// why not, or `None` for a delivery.
    pub(super) fn on_send_result(
        &mut self,
        id: u16,
        undelivered: Option<Undelivered>,
        now: Now,
        batch: &mut ActionBatch,
    ) {
        // An unknown id predates a reconnect or answers something else. Nothing to resolve.
        let Some(pending) = self.pending.remove(&id) else { return };
        match pending {
            Pending::Admin(pending) => {
                let outcome =
                    if undelivered.is_none() { AdminOutcome::Acked } else { AdminOutcome::Unacked };
                // How long the bus took to deliver it, on this host's clock: from the heartbeat
                // that sent it to the bridge's answer.
                let latency_us = (outcome == AdminOutcome::Acked).then(|| {
                    u32::try_from(now.mono.duration_since(pending.sent_mono).as_micros())
                        .unwrap_or(u32::MAX)
                });
                self.resolve(&pending, outcome, latency_us, now, batch);
            }
            Pending::Clear { mac, .. } => {
                // Undelivered leaves the flag set for the next heartbeat to retry.
                if undelivered.is_none()
                    && let Some(node) = self.nodes.get_mut(&mac)
                {
                    node.clear_dedup_ring = false;
                }
            }
        }
    }

    /// Give up on assignments the bridge never answered for. An expired clear is dropped quietly,
    /// flag still set, so the next heartbeat retries it.
    pub(super) fn expire_pending(&mut self, now: Now, batch: &mut ActionBatch) {
        let timeout = self.config.admin_timeout;
        let stale: Vec<u16> = self
            .pending
            .iter()
            .filter(|(_, p)| now.mono.duration_since(p.sent_mono()) >= timeout)
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            if let Some(Pending::Admin(pending)) = self.pending.remove(&id) {
                self.resolve(&pending, AdminOutcome::Silent, None, now, batch);
            }
        }
    }

    /// Record an assignment attempt's outcome, and update what the node is believed to hold.
    fn resolve(
        &mut self,
        pending: &PendingAdmin,
        outcome: AdminOutcome,
        latency_us: Option<u32>,
        now: Now,
        batch: &mut ActionBatch,
    ) {
        let acked = outcome == AdminOutcome::Acked;
        if acked {
            self.counters.admin_acked += 1;
        } else {
            self.counters.admin_failed += 1;
        }

        if let Some(node) = self.nodes.get_mut(&pending.mac) {
            // Attempts resolve in completion order, not send order. A failure the node has moved
            // past still gets its row, but does not overwrite what landed afterwards.
            let superseded =
                node.confirmed.is_some_and(|c| c.counter >= pending.assignment.counter);
            if acked {
                node.last_outcome = Some(outcome);
                node.last_latency_us = latency_us;
                // Cleared on delivery, not on enqueue, and only if the node still wants what was
                // sent. Otherwise it is already dirty again under a newer epoch.
                if node.desired.is_some_and(|d| d.counter == pending.assignment.counter) {
                    node.confirmed = Some(pending.assignment);
                    node.dirty = false;
                }
            } else if !superseded {
                node.last_outcome = Some(outcome);
            }
        }

        batch.records.push(Record::Assignment(AssignmentSent {
            node_mac: pending.mac,
            counter: pending.assignment.counter,
            wire_version: wire_epoch(pending.assignment.counter),
            channels: pending.assignment.channels,
            ble: pending.assignment.ble,
            created_at_ms: pending.sent_ms,
            // `Silent` means nothing came back, so there is no delivery to stamp. A time here would
            // be the timeout posing as an answer.
            delivered_at_ms: (outcome != AdminOutcome::Silent).then_some(now.unix_ms),
            outcome,
            latency_us,
        }));
    }
}

/// A record for the node in `slot`.
fn to_node(slot: Slot, id: u16, kind: DownType, body: &[u8]) -> HostToBridge {
    // An assignment and a clear are a few bytes against a 1029-byte body.
    let body = ToNodeBody::from_slice(body).unwrap_or_default();
    HostToBridge::ToNode { slot, id, kind: kind.as_u8(), body }
}
