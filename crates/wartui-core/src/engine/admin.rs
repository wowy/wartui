use std::time::Instant;

use wartui_proto::air::{AdminMsg, ClearMsg, wire_epoch};
use wartui_proto::link::{EspNowPayload, HostToBridge, SendStatus};
use wartui_proto::mac::Mac;
use wartui_proto::node;

use super::{ActionBatch, Assignment, FleetEngine, Now};
use crate::record::{AdminOutcome, AssignmentSent, Record};

/// One assignment or clear in flight.
#[derive(Debug, Clone, Copy)]
pub(super) enum Pending {
    Admin(PendingAdmin),
    Clear { mac: Mac, sent_mono: Instant },
}

impl Pending {
    /// When this was put on the air, whichever variant it is.
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
    /// The bridge's stamp on the heartbeat that opened the window, so latency is measured on the
    /// bridge's clock alone, free of host scheduling noise.
    heartbeat_rx_us: Option<u32>,
}

impl FleetEngine {
    /// Put a dirty node's assignment on the air. Called only straight off a heartbeat, the one
    /// moment the node listens on the control channel.
    pub(super) fn send_admin(&mut self, mac: Mac, now: Now, batch: &mut ActionBatch) {
        let id = self.next_send_id();

        // Read before borrowing the node, acted on after. Only windows that owed something count,
        // or they would bury the ones that did.
        let live = self.air_is_live();

        let Some(node) = self.nodes.get_mut(&mac) else { return };
        if !node.dirty {
            return;
        }
        let Some(assignment) = node.desired else { return };
        if !live {
            self.counters.admin_windows_missed += 1;
            return;
        }
        node.admin_attempts += 1;
        let heartbeat_rx_us = node.last_heartbeat_rx_us;

        let msg = AdminMsg {
            epoch: wire_epoch(assignment.counter),
            flags: AdminMsg::flags_for(assignment.ble),
            channels: assignment.channels,
            tx_power: assignment.tx_power,
        };
        // Fifteen bytes into a 250-byte buffer, so this cannot fail.
        let payload = EspNowPayload::from_slice(&msg.encode()).unwrap_or_default();

        self.pending.insert(
            id,
            Pending::Admin(PendingAdmin {
                mac,
                assignment,
                sent_mono: now.mono,
                sent_ms: now.unix_ms,
                heartbeat_rx_us,
            }),
        );
        self.counters.admin_sent += 1;
        if let Some(node) = self.nodes.get_mut(&mac) {
            node.peered = true;
        }
        batch.urgent.push(HostToBridge::SendEspNow {
            id,
            dst: mac,
            // Add if absent. Removing it here would race the transmit callback, so
            // `evict_stale_peers` does it once the node is gone.
            ensure_peer: true,
            payload,
        });
    }

    /// Put a clear on the air for a node that owes one. Called only off a heartbeat, as
    /// [`Self::send_admin`] is.
    pub(super) fn send_dedup_ring_clear(&mut self, mac: Mac, now: Now, batch: &mut ActionBatch) {
        let live = self.air_is_live();
        let Some(node) = self.nodes.get(&mac) else { return };
        if !node.clear_dedup_ring || !live {
            return;
        }

        let id = self.next_send_id();
        self.pending.insert(id, Pending::Clear { mac, sent_mono: now.mono });
        // Six bytes into a 250-byte buffer, so this cannot fail.
        let payload = EspNowPayload::from_slice(&ClearMsg.encode()).unwrap_or_default();
        if let Some(node) = self.nodes.get_mut(&mac) {
            node.peered = true;
        }
        batch.urgent.push(HostToBridge::SendEspNow { id, dst: mac, ensure_peer: true, payload });
    }

    fn next_send_id(&mut self) -> u16 {
        let id = self.next_send_id;
        self.next_send_id = self.next_send_id.wrapping_add(1).max(1);
        id
    }

    /// Handle a full peer table, for assignments and clears alike. It stays full until a stale peer
    /// is evicted or the bridge announces itself, and both clear the refusal.
    fn mark_peer_table_full(&mut self, mac: Mac) {
        self.counters.peer_table_full += 1;
        if let Some(node) = self.nodes.get_mut(&mac) {
            // The node leaves the plan, so the next re-cut spreads the pool over reachable nodes.
            node.peer_refused = true;
            // The bridge refused the slot, so there is none to evict.
            node.peered = false;
        }
    }

    /// The bridge said what became of an assignment or a clear.
    pub(super) fn on_send_result(
        &mut self,
        id: u16,
        status: SendStatus,
        tx_us: u32,
        now: Now,
        batch: &mut ActionBatch,
    ) {
        // An unknown id predates a reconnect or answers something else. Nothing to resolve.
        let Some(pending) = self.pending.remove(&id) else { return };
        match pending {
            Pending::Admin(pending) => {
                self.on_admin_send_result(pending, status, tx_us, now, batch)
            }
            Pending::Clear { mac, .. } => self.on_clear_send_result(mac, status),
        }
    }

    /// The bridge said what became of an assignment.
    fn on_admin_send_result(
        &mut self,
        pending: PendingAdmin,
        status: SendStatus,
        tx_us: u32,
        now: Now,
        batch: &mut ActionBatch,
    ) {
        let outcome = match status {
            SendStatus::AckOk => AdminOutcome::Acked,
            SendStatus::AckFail => AdminOutcome::Unacked,
            // Broadcast is never acked, and assignments are always unicast, so it means a bad
            // address.
            SendStatus::Broadcast
            | SendStatus::NoPeer
            | SendStatus::PeerTableFull
            | SendStatus::Rejected => AdminOutcome::Refused,
        };

        // A full peer table is terminal: give up on what was wanted, and let the view say why.
        if matches!(status, SendStatus::PeerTableFull) {
            self.mark_peer_table_full(pending.mac);
            if let Some(node) = self.nodes.get_mut(&pending.mac) {
                node.dirty = false;
                node.desired = None;
            }
        }

        // Both stamps are the bridge's microsecond clock, which wraps about every 71 minutes.
        // Wrapping subtraction is correct across it.
        //
        // The column means how long the assignment took to land inside a heartbeat's window. A
        // larger figure is not that, so it is not recorded as one. It comes from a heartbeat
        // replayed out of a backlog: honest arithmetic that says nothing about how fast the bridge
        // answered. `None` is the truthful answer, and the outcome is recorded either way.
        let latency_us = pending
            .heartbeat_rx_us
            .map(|rx_us| tx_us.wrapping_sub(rx_us))
            .filter(|us| *us <= node::ADMIN_WAIT_MS * 1_000);

        self.resolve(&pending, outcome, latency_us, now, batch);
    }

    /// The bridge said what became of a clear. Clears write no store record and touch no `admin_*`
    /// counter.
    fn on_clear_send_result(&mut self, mac: Mac, status: SendStatus) {
        match status {
            SendStatus::AckOk => {
                if let Some(node) = self.nodes.get_mut(&mac) {
                    node.clear_dedup_ring = false;
                }
            }
            SendStatus::PeerTableFull => {
                self.mark_peer_table_full(mac);
                if let Some(node) = self.nodes.get_mut(&mac) {
                    node.clear_dedup_ring = false;
                }
            }
            // Anything else leaves the flag set for the next heartbeat to retry.
            SendStatus::AckFail
            | SendStatus::Broadcast
            | SendStatus::NoPeer
            | SendStatus::Rejected => {}
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
                // Cleared on the MAC-layer ack, not on enqueue, and only if the node still wants
                // what was sent. Otherwise it is already dirty again under a newer epoch.
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
