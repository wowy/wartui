use wartui_proto::bus::{
    ACK_TIMEOUT_MS, Capabilities, DecodeError, Heartbeat as HeartbeatMsg, NODE_VERSION, RecordKind,
    SightingBatch, SightingMsg, Slot, UpType, wire_epoch,
};
use wartui_proto::mac::{self, Mac};
use wartui_proto::plan::SCAN_CHANNELS;

#[cfg(doc)]
use super::Counters;
use super::{ActionBatch, FleetEngine, NodeState, Now};
use crate::record::{BatchGap, Heartbeat, NodeSeen, Observation, RawFrame, Record};

/// Log a node's boot at `warn!` when its [`ResetCause`](wartui_proto::reset::ResetCause) is a
/// fault, so a brownout, watchdog, lockup or panic mid-drive reaches the operator, and at
/// `info!` when it was a power-on or a reset over USB.
macro_rules! by_reset {
    ($cause:expr, $($arg:tt)*) => {
        if $cause.is_fault() { tracing::warn!($($arg)*) } else { tracing::info!($($arg)*) }
    };
}

/// How long after a batch a byte-identical same-`seq` repeat still counts as a copy of it.
///
/// A copy is the node bus resending a batch whose ack the node did not see. It is dropped and
/// counted in [`Counters::duplicate_batches`]. Copies of one batch arrive at most one arm wait
/// plus one ack timeout apart, each [`ACK_TIMEOUT_MS`] on the node, so the window is twice that,
/// compared with a strict `<`. Each copy refreshes the stamp, so the window runs from the last.
///
/// A node that gives up on a batch discards it and starts its next under the same `seq`, with
/// new bytes, so that batch is recorded. A bridge reboot resets `rx_us`, so the wrapped difference
/// is huge and the batch is recorded: the safe direction.
pub(super) const DUPLICATE_BATCH_WINDOW_US: u64 = 2 * ACK_TIMEOUT_MS * 1_000;

/// The smallest `seq` gap not counted as loss: one this large is read as a wrap or reordering
/// ([`FleetEngine::note_batch_seq`]).
const UNCOUNTED_GAP: u16 = 1024;

/// A since-boot count's contribution: nothing for a baseline, the whole value after a restart or a
/// fall, the difference otherwise.
///
/// Only a restart makes a count fall, so a fall counts as one even when `restarted` misses it. A
/// real u16 wrap reads the same, undercounting at most one heartbeat's worth every 65536 rather
/// than adding ~65000.
pub(crate) fn advance_since_boot(value: u64, prev: Option<u64>, restarted: bool) -> u64 {
    match prev {
        None => 0,
        Some(prev) if restarted || value < prev => value,
        Some(prev) => value - prev,
    }
}

/// One record a node sent, as the bridge forwarded it.
#[derive(Debug, Clone, Copy)]
pub(super) struct FromNode<'a> {
    pub(super) slot: Slot,
    /// The bridge's microsecond stamp on arrival.
    pub(super) rx_us: u32,
    pub(super) kind: u8,
    pub(super) body: &'a [u8],
}

impl FleetEngine {
    pub(super) fn on_from_node(&mut self, rx: &FromNode<'_>, now: Now, batch: &mut ActionBatch) {
        let FromNode { slot, kind, body, .. } = *rx;
        self.counters.frames += 1;

        if self.config.record_raw {
            batch.records.push(Record::Raw(RawFrame {
                rx_at_ms: now.unix_ms,
                slot,
                kind,
                bytes: body.to_vec(),
            }));
        }

        match UpType::from_u8(kind) {
            Some(UpType::Heartbeat) => match HeartbeatMsg::decode(body) {
                // The right length is no guarantee of the right layout: a minor version may move
                // fields without resizing the body.
                Ok(heartbeat) if !heartbeat.capabilities.version.compat_with_host().drivable() => {
                    self.on_outdated_heartbeat(
                        rx,
                        heartbeat.mac,
                        heartbeat.capabilities,
                        now,
                        batch,
                    );
                }
                Ok(heartbeat) => self.on_heartbeat(rx, heartbeat, now, batch),
                // A heartbeat of another length still opens with the prefix and the MAC. From a
                // node on another major or minor that names a node one side must update; from one
                // on this minor it is a bug, counted like any other mismatch.
                Err(DecodeError::BadLength { .. }) => match HeartbeatMsg::identity(body) {
                    Ok((caps, mac)) if !caps.version.compat_with_host().drivable() => {
                        self.on_outdated_heartbeat(rx, mac, caps, now, batch);
                    }
                    _ => self.counters.incompatible += 1,
                },
                Err(_) => self.counters.undecodable += 1,
            },
            Some(UpType::Sightings) => {
                // A batch names no node: the slot's latest heartbeat does.
                let Some(src) = self.slots.get(&slot).copied() else {
                    self.counters.unknown_slot += 1;
                    return;
                };
                // A node on another major or minor may still hold an assignment from a build that
                // could drive it, and its batches are not this build's layout even when they
                // parse.
                if !self.drivable(src) {
                    self.counters.incompatible += 1;
                    return;
                }
                match SightingBatch::decode(body) {
                    Ok(sightings) => self.on_sightings(rx, src, sightings, now, batch),
                    Err(DecodeError::BadLength { .. }) => self.counters.incompatible += 1,
                    Err(_) => self.counters.undecodable += 1,
                }
            }
            // A node's update status is the update session's to read, not the engine's.
            Some(UpType::UpdateStatus) => {}
            None => self.counters.undecodable += 1,
        }
    }

    /// File the node a heartbeat from `slot` names as that slot's.
    ///
    /// A known node on a new slot moves there, and a slot whose heartbeat names a new node is
    /// that node's from now on. The node that held it keeps its MAC and its row, but has no slot
    /// until its own heartbeat names one, so nothing is sent to the wrong node.
    fn take_slot(&mut self, slot: Slot, mac: Mac) {
        let previous = self.slots.insert(slot, mac);
        if let Some(other) = previous.filter(|other| *other != mac)
            && let Some(node) = self.nodes.get_mut(&other)
            && node.slot == Some(slot)
        {
            node.slot = None;
        }
        let Some(node) = self.nodes.get_mut(&mac) else { return };
        let moved_from = node.slot.replace(slot).filter(|old| *old != slot);
        if let Some(old) = moved_from {
            if self.slots.get(&old) == Some(&mac) {
                self.slots.remove(&old);
            }
            let (from, to) = (self.slot_label(old), self.slot_label(slot));
            tracing::info!(mac = %mac::full(&mac), %from, %to, "node moved slot");
        }
    }

    /// A heartbeat: proof of life, the node's slot, and the moment to answer it.
    fn on_heartbeat(
        &mut self,
        rx: &FromNode<'_>,
        heartbeat: HeartbeatMsg,
        now: Now,
        batch: &mut ActionBatch,
    ) {
        let FromNode { slot, .. } = *rx;
        let label = self.slot_label(slot);
        let src = heartbeat.mac;
        self.see_node(src, now, Some(heartbeat.capabilities), batch);
        self.take_slot(slot, src);
        self.counters.heartbeats += 1;
        // Read before borrowing the node: liveness decides whether its epoch is believed.
        let live = self.reading_live();
        let node = self.nodes.entry(src).or_insert_with(|| NodeState::new(src, now));
        // A falling counter means the node restarted and forgot its assignment.
        let counter_rebooted = node.counter.is_some_and(|previous| heartbeat.counter < previous);
        // So does a live heartbeat reporting no epoch after a real one: the epoch reset at boot
        // too.
        let epoch_rebooted =
            live && heartbeat.epoch == 0 && node.held_epoch.is_some_and(|previous| previous != 0);
        let rebooted = counter_rebooted || epoch_rebooted;
        // `see_node` leaves `last_heartbeat` alone, so this is still the previous beat.
        match node.last_heartbeat {
            None => by_reset!(
                heartbeat.reset_cause,
                mac = %mac::full(&src),
                slot = %label,
                capabilities = %heartbeat.capabilities,
                reset = %heartbeat.reset_cause,
                vdd = %heartbeat.vdd,
                die_temp = %heartbeat.die_temp,
                "node joined"
            ),
            // Only after a logged departure: nodes that never left heartbeat again after a link
            // outage.
            Some(last) if node.departed => {
                node.departed = false;
                let silent_ms = now.mono.duration_since(last).as_millis() as u64;
                tracing::info!(mac = %mac::full(&src), slot = %label, silent_ms, "node returned");
            }
            Some(_) => {}
        }
        if rebooted {
            by_reset!(
                heartbeat.reset_cause,
                mac = %mac::full(&src),
                slot = %label,
                counter_before = node.counter,
                counter_after = heartbeat.counter,
                reset = %heartbeat.reset_cause,
                vdd = %heartbeat.vdd,
                die_temp = %heartbeat.die_temp,
                "node restarted"
            );
            node.reboots += 1;
            // Only restarts seen here: a join's boot may predate the run.
            if heartbeat.reset_cause.is_fault() {
                node.fault_restart = Some(heartbeat.reset_cause);
                node.fault_restarts += 1;
            }
            // The node forgot it. `reissue` below re-sends it.
            node.confirmed = None;
            // Booting empties the ring.
            node.clear_dedup_ring = false;
            // `seq` restarted too, so the gap since the last batch is history, not loss. An
            // identical batch after a reboot is new, not a retransmission.
            node.last_seq = None;
            node.last_seq_live = false;
            node.last_batch = None;
            node.last_batch_rx_us = None;
            // The epoch reset too, so what it holds is unknown.
            node.held_epoch = None;
        }
        node.capabilities = Some(heartbeat.capabilities);
        // A replayed heartbeat's epoch is history and must not overwrite a live one.
        if live {
            node.held_epoch = Some(heartbeat.epoch);
        }
        // Since-boot counts to session counts (`advance_since_boot`). The first heartbeat is a
        // baseline, like the bridge's `dropped_baseline`: earlier refusals are not this capture's.
        // A falling count also reads as a restart, since a node rebooting out of range can return
        // with a higher counter. That decides only drop counting, not `reboots`.
        let advance = |value: u16, prev: Option<u16>| {
            advance_since_boot(u64::from(value), prev.map(u64::from), rebooted)
        };
        self.counters.wifi_refused += advance(heartbeat.wifi_refused, node.last_wifi_refused);
        self.counters.ble_refused += advance(heartbeat.ble_refused, node.last_ble_refused);
        node.last_wifi_refused = Some(heartbeat.wifi_refused);
        node.last_ble_refused = Some(heartbeat.ble_refused);
        node.counter = Some(heartbeat.counter);
        node.last_heartbeat = Some(now.mono);
        // Back on the bus: this heartbeat came through its slot.
        node.off_bus = false;
        node.heartbeats += 1;
        node.vdd = heartbeat.vdd;
        if node.vdd_baseline.mv().is_none() {
            node.vdd_baseline = heartbeat.vdd;
        }
        node.die_temp = heartbeat.die_temp;
        batch.records.push(Record::Heartbeat(Heartbeat {
            node_mac: src,
            rx_at_ms: now.unix_ms,
            counter: heartbeat.counter,
            epoch: heartbeat.epoch,
            slot,
            wifi_refused: heartbeat.wifi_refused,
            ble_refused: heartbeat.ble_refused,
            reset_cause: heartbeat.reset_cause,
            vdd_mv: heartbeat.vdd.mv(),
            die_temp_c: heartbeat.die_temp.c(),
            heap_free: heartbeat.heap_free,
            live,
        }));

        if rebooted && node.desired.is_some() {
            self.reissue(src);
        }
        // Delivered but not adopted: the bus delivered the record, but the heartbeat says the node
        // does not hold it (a length or version mismatch, a dropped inbox, or a bug). After a
        // reboot `confirmed` is already `None`, so that case stays out. Re-sent on this heartbeat
        // like an undelivered assignment, under the same epoch, which the node does not hold either
        // way.
        //
        // Re-fetched because `reissue` above borrowed all of `self`.
        if let Some(node) = self.nodes.get_mut(&src)
            && live
            && !node.dirty
            && let Some(confirmed) = node.confirmed
            && heartbeat.epoch != wire_epoch(confirmed.counter)
        {
            node.unadopted += 1;
            node.dirty = true;
            self.counters.admin_unadopted += 1;
        }
        // A first heartbeat joins the fleet and changes every node's range. Re-partitioning now
        // lets the node take its share in answer to this heartbeat.
        self.replan(now);
        // The node holds exactly what `desired` carries, yet it is still dirty. Either `deal`
        // allocated before `held_epoch` was known (a backlog replay leaves it `None`), or a
        // delivery was lost after adoption. Re-issuing spends one epoch and is right either way.
        if let Some(node) = self.nodes.get(&src)
            && live
            && node.dirty
            && let Some(desired) = node.desired
            && node.held_epoch == Some(wire_epoch(desired.counter))
        {
            self.reissue(src);
        }
        // Answered off the heartbeat, through the slot it came from. A heartbeat read behind sends
        // nothing.
        self.send_admin(src, now, batch);
        self.send_dedup_ring_clear(src, now, batch);
    }

    /// Whether the node at `src` runs firmware this build can drive, as far as its last heartbeat
    /// said. A node not yet heard from by heartbeat is given the benefit of the doubt.
    fn drivable(&self, src: Mac) -> bool {
        self.nodes
            .get(&src)
            .and_then(|node| node.capabilities)
            .is_none_or(|caps| caps.version.compat_with_host().drivable())
    }

    /// A heartbeat from a node on another major or minor, of which only the prefix and MAC are
    /// read.
    ///
    /// The node is filed and shown with its version so the operator can find it, but it is never
    /// assignable ([`FleetEngine::is_assignable`]): nothing past the prefix is this build's
    /// layout, so its counter, epoch and refusals mean nothing here. It is answered with nothing
    /// and sets no `last_heartbeat`. If it holds the Bluetooth scan, the scan is withdrawn now rather
    /// than when it ages out, and the bridge's Bluetooth slot stays as it is, so the node takes the
    /// scan back once it can be driven again.
    fn on_outdated_heartbeat(
        &mut self,
        rx: &FromNode<'_>,
        src: Mac,
        capabilities: Capabilities,
        now: Now,
        batch: &mut ActionBatch,
    ) {
        self.see_node(src, now, Some(capabilities), batch);
        // Its slot is still its own: an update goes to it there.
        self.take_slot(rx.slot, src);
        let label = self.slot_label(rx.slot);
        let Some(node) = self.nodes.get_mut(&src) else { return };
        if node.capabilities.map(|caps| caps.version) != Some(capabilities.version) {
            let mac = mac::full(&src);
            let version = capabilities.version;
            if capabilities.version.compat_with_host().node_behind() {
                tracing::warn!(
                    %mac,
                    slot = %label,
                    %version,
                    host = %NODE_VERSION,
                    "node needs update"
                );
            } else {
                tracing::warn!(
                    %mac,
                    slot = %label,
                    %version,
                    host = %NODE_VERSION,
                    "host needs update to drive this node"
                );
            }
        }
        node.capabilities = Some(capabilities);
        node.heartbeats += 1;
        // A node that was in the plan on a compatible build leaves it now, and with it the
        // Bluetooth scan: `replan` gives the scan only to an assignable node.
        self.replan(now);
    }

    /// A batch of sightings, recorded unless it retransmits the batch before it.
    fn on_sightings(
        &mut self,
        rx: &FromNode<'_>,
        src: Mac,
        sightings: SightingBatch<'_>,
        now: Now,
        batch: &mut ActionBatch,
    ) {
        let FromNode { rx_us, body: payload, .. } = *rx;
        // Once per batch, not once per sighting.
        self.see_node(src, now, None, batch);
        if self.is_duplicate_batch(src, sightings.seq, payload, rx_us) {
            // A copy of the batch before. `see_node` already took it as proof of life. Recording
            // it again would double the observations.
            self.counters.duplicate_batches += 1;
            if let Some(node) = self.nodes.get_mut(&src) {
                node.duplicate_batches += 1;
            }
        } else {
            self.note_batch_seq(src, sightings.seq, now, batch);
            for (sighting, raw) in sightings.iter() {
                self.on_sighting(rx, src, sighting, raw, now, batch);
            }
        }
        self.remember_batch(src, payload, rx_us);
    }

    /// One record of a sighting batch, after [`Self::see_node`] and [`Self::note_batch_seq`] ran
    /// for its batch.
    fn on_sighting(
        &mut self,
        rx: &FromNode<'_>,
        src: Mac,
        sighting: SightingMsg<'_>,
        raw: &[u8],
        now: Now,
        batch: &mut ActionBatch,
    ) {
        let slot = rx.slot;
        // An excluded network is never captured: dropped before it counts, reaches the tail or is
        // recorded. Its batch already counted for liveness and sequence.
        if sighting.kind == RecordKind::Wifi && self.config.excluded_ssids.contains(sighting.ssid) {
            return;
        }
        self.counters.observations += 1;
        match sighting.kind {
            RecordKind::Wifi => {
                self.counters.wifi_observations += 1;
                self.unique_wifi.insert(&sighting.bssid);
                // A channel not in the table counts nowhere.
                if let Some(i) = SCAN_CHANNELS.iter().position(|&c| c == sighting.channel) {
                    self.wifi_by_channel[i] += 1;
                }
            }
            RecordKind::Ble => {
                self.counters.ble_observations += 1;
                self.unique_ble.insert(&sighting.bssid);
            }
        }
        if let Some(node) = self.nodes.get_mut(&src) {
            node.observations += 1;
        }
        // The trailer's meaning depends on the kind: roaming consortium body for Wi-Fi, company
        // identifier for BLE. A BLE trailer that is not two bytes cannot come from a well-formed
        // batch of this build, so it is dropped rather than guessed at.
        let (rcoi, mfgr_id) = match sighting.kind {
            RecordKind::Wifi => ((!sighting.ext.is_empty()).then(|| sighting.ext.to_vec()), None),
            RecordKind::Ble => (
                None,
                (sighting.ext.len() == 2)
                    .then(|| u16::from_le_bytes([sighting.ext[0], sighting.ext[1]])),
            ),
        };
        let observation = Observation {
            node_mac: src,
            rx_at_ms: now.unix_ms,
            slot,
            bssid: sighting.bssid,
            ssid: sighting.ssid.to_vec(),
            security: sighting.security.to_string(),
            channel: u16::from(sighting.channel),
            rssi: i16::from(sighting.rssi),
            kind: sighting.kind,
            rcoi,
            mfgr_id,
            fix: self.config.position.resolve(now.unix_ms),
            // One record's bytes. The batch header carries nothing a re-parse needs, and
            // `--record-raw` keeps the whole batch in `Record::Raw`.
            raw_body: raw.to_vec(),
        };
        self.push_tail(&observation);
        batch.records.push(Record::Observation(observation));
    }

    /// Track batches lost between this node and the host.
    ///
    /// `seq` counts up once per batch the P4 acknowledges, wraps, and restarts at boot
    /// ([`Self::on_heartbeat`] resets the baseline). A retransmission never reaches here
    /// ([`Self::is_duplicate_batch`]). A repeated `seq` that does is a send the node gave up on
    /// though the batch arrived. The node reuses that `seq` for its next batch, so no loss is
    /// counted.
    ///
    /// A gap under [`UNCOUNTED_GAP`] is that many batches lost. At or past it the count wrapped or
    /// the batch came out of order, and guessing would invent history. Each counted gap is also a
    /// [`Record::BatchGap`], so the store's sum matches the live count.
    ///
    /// Only a gap after a live batch counts. One after a replayed batch spans a spell the host
    /// fell behind, whose losses are the bridge's `dropped_tx`. The few batches lost at the
    /// replay-to-live handoff go uncounted.
    fn note_batch_seq(&mut self, src: Mac, seq: u16, now: Now, batch: &mut ActionBatch) {
        // Read before borrowing the node. See `reading_live`.
        let live = self.reading_live();
        let Some(node) = self.nodes.get_mut(&src) else { return };
        if let Some(last) = node.last_seq.filter(|_| node.last_seq_live) {
            let gap = seq.wrapping_sub(last.wrapping_add(1));
            let counted = gap < UNCOUNTED_GAP;
            if counted {
                node.batches_lost += u64::from(gap);
                self.counters.batches_lost += u64::from(gap);
            }
            if counted && gap > 0 {
                batch.records.push(Record::BatchGap(BatchGap {
                    node_mac: src,
                    rx_at_ms: now.unix_ms,
                    after_seq: last,
                    seq,
                    lost: gap,
                }));
            }
        }
        node.last_seq = Some(seq);
        node.last_seq_live = live;
    }

    /// Whether `payload` at `rx_us` retransmits this node's latest batch: same `seq`, same bytes,
    /// inside [`DUPLICATE_BATCH_WINDOW_US`]. A same-`seq` batch with different bytes is
    /// [`Self::note_batch_seq`]'s business.
    fn is_duplicate_batch(&self, src: Mac, seq: u16, payload: &[u8], rx_us: u32) -> bool {
        self.nodes.get(&src).is_some_and(|node| {
            node.last_seq == Some(seq)
                && node.last_batch.as_deref() == Some(payload)
                && node.last_batch_rx_us.is_some_and(|last_rx_us| {
                    u64::from(rx_us.wrapping_sub(last_rx_us)) < DUPLICATE_BATCH_WINDOW_US
                })
        })
    }

    /// Remember this batch for the next [`Self::is_duplicate_batch`]. Runs for every batch,
    /// duplicate or not.
    fn remember_batch(&mut self, src: Mac, payload: &[u8], rx_us: u32) {
        if let Some(node) = self.nodes.get_mut(&src) {
            node.last_batch = Some(payload.to_vec());
            node.last_batch_rx_us = Some(rx_us);
        }
    }

    /// Refresh what is known about the node at `src`, and file its row. Split out because a
    /// sighting proves a node exists but its payload is a network: filing that as an identity would
    /// name a node after what it heard.
    fn see_node(
        &mut self,
        src: Mac,
        now: Now,
        capabilities: Option<Capabilities>,
        batch: &mut ActionBatch,
    ) {
        let node = self.nodes.entry(src).or_insert_with(|| NodeState::new(src, now));
        node.last_seen = now.mono;
        node.last_seen_ms = now.unix_ms;
        batch.records.push(Record::Node(NodeSeen {
            mac: src,
            first_seen_ms: node.first_seen_ms,
            last_seen_ms: now.unix_ms,
            capabilities: capabilities.map(|caps| caps.to_string()),
        }));
    }
}
