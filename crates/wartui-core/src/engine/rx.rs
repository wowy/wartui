use wartui_proto::air::{
    Capabilities, DecodeError, Frame, HeartbeatMsg, RecordKind, SightingBatch, SightingMsg,
    foreign, wire_epoch,
};
use wartui_proto::mac::{self, Mac};
use wartui_proto::node;

#[cfg(doc)]
use super::Counters;
use super::{ActionBatch, FleetEngine, NodeState, Now};
use crate::record::{BatchGap, Heartbeat, NodeSeen, Observation, RawFrame, Record};

/// How long after a batch a byte-identical same-`seq` repeat still counts as an 802.11 retry, not a
/// node's own re-send.
///
/// A retry follows a lost MAC ack: the radio retransmits and the bridge delivers every copy. It is
/// dropped and counted in [`Counters::duplicate_batches`]. A re-send follows a send whose every ack
/// was lost: the node leaves `seq` unchanged and may next report the same addresses at the same
/// RSSI. That is a real sighting and is recorded.
///
/// 100 ms, compared with a strict `<`. The radio's retries arrived about 4 ms after the original,
/// all within 100 ms (`docs/batch-loss-findings.md`). A re-send takes at least a whole sweep, which
/// has no admin window, so two reports of one channel are at least a dwell apart
/// (`node::CHANNEL_DWELL_MS`). A one-channel share re-reports every ~130 ms. A bridge reboot resets
/// `rx_us`, so the wrapped difference is huge and the batch is recorded: the safe direction.
pub(super) const DUPLICATE_BATCH_WINDOW_US: u64 = 100_000;

const _: () = assert!(
    DUPLICATE_BATCH_WINDOW_US < node::CHANNEL_DWELL_MS as u64 * 1_000,
    "the shortest time between two reports of one channel is one dwell"
);

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

/// One frame the bridge received, as it reported it.
#[derive(Debug, Clone, Copy)]
pub(super) struct Rx<'a> {
    pub(super) src: Mac,
    pub(super) dst: Mac,
    pub(super) rssi: i8,
    /// The bridge's microsecond stamp on arrival.
    pub(super) rx_us: u32,
    pub(super) payload: &'a [u8],
}

impl FleetEngine {
    pub(super) fn on_rx(&mut self, rx: &Rx<'_>, now: Now, batch: &mut ActionBatch) {
        let Rx { src, dst, rssi, payload, .. } = *rx;
        self.counters.frames += 1;

        if self.config.record_raw {
            batch.records.push(Record::Raw(RawFrame {
                rx_at_ms: now.unix_ms,
                src,
                dst,
                rssi: Some(rssi),
                bytes: payload.to_vec(),
            }));
        }

        let frame = match Frame::decode(payload) {
            Ok(frame) => frame,
            // Not ours: no frame preamble.
            Err(DecodeError::BadMagic) => {
                match foreign::classify(payload) {
                    Some(foreign::Foreign::Admin) => self.counters.foreign_admin += 1,
                    Some(foreign::Foreign::Node) => self.counters.foreign_fleet += 1,
                    None => self.counters.undecodable += 1,
                }
                return;
            }
            // Version or length mismatch.
            Err(DecodeError::BadVersion(_) | DecodeError::BadLength { .. }) => {
                self.counters.incompatible += 1;
                return;
            }
            Err(_) => {
                self.counters.undecodable += 1;
                return;
            }
        };

        // Decode before admitting anyone to the fleet.
        match frame {
            // Nothing to do, but an operator chasing a fleet that keeps changing its mind needs to
            // know. Foreign clears count the same as foreign assignments.
            Frame::Admin(_) | Frame::Clear(_) => self.counters.foreign_admin += 1,
            Frame::Heartbeat(heartbeat) => {
                self.on_heartbeat(rx, heartbeat, now, batch);
            }
            Frame::Sightings(sightings) => {
                self.on_sightings(rx, sightings, now, batch);
            }
        }
    }

    /// A heartbeat: proof of life, and the one moment the node's admin window is open.
    fn on_heartbeat(
        &mut self,
        rx: &Rx<'_>,
        heartbeat: HeartbeatMsg,
        now: Now,
        batch: &mut ActionBatch,
    ) {
        let Rx { src, rssi, rx_us, .. } = *rx;
        self.see_node(src, now, rssi, Some(heartbeat.capabilities), batch);
        self.counters.heartbeats += 1;
        // Read before borrowing the node: liveness decides whether its epoch is believed.
        let live = self.air_is_live();
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
            None => tracing::info!(
                mac = %mac::full(&src),
                capabilities = %heartbeat.capabilities,
                rssi,
                "node joined"
            ),
            Some(last) if now.mono.duration_since(last) >= self.config.topology_timeout => {
                let silent_ms = now.mono.duration_since(last).as_millis() as u64;
                tracing::info!(mac = %mac::full(&src), silent_ms, "node returned");
            }
            Some(_) => {}
        }
        if rebooted {
            tracing::info!(
                mac = %mac::full(&src),
                counter_before = node.counter,
                counter_after = heartbeat.counter,
                beat = heartbeat.beat,
                "node restarted"
            );
            node.reboots += 1;
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
        node.last_heartbeat_rx_us = Some(rx_us);
        node.heartbeats += 1;
        batch.records.push(Record::Heartbeat(Heartbeat {
            node_mac: src,
            rx_at_ms: now.unix_ms,
            counter: heartbeat.counter,
            epoch: heartbeat.epoch,
            link_rssi: Some(rssi),
            wifi_refused: heartbeat.wifi_refused,
            ble_refused: heartbeat.ble_refused,
            beat: heartbeat.beat,
            unsent: heartbeat.unsent,
            live,
        }));

        if rebooted && node.desired.is_some() {
            self.reissue(src);
        }
        // Acked but not adopted: the radio delivered the frame, but the heartbeat says the node
        // does not hold it (a length or version mismatch, a dropped receive queue, or a bug). After
        // a reboot `confirmed` is already `None`, so that case stays out. Re-sent in this window
        // like an unacknowledged assignment, under the same epoch, which the node does not hold
        // either way.
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
        // lets the node take its share in the window it just opened.
        self.replan(now);
        // The node holds exactly what `desired` carries, yet it is still dirty. Either `deal`
        // allocated before `held_epoch` was known (a backlog replay leaves it `None`), or an ack
        // was lost after adoption. Re-issuing spends one epoch and is right either way.
        if let Some(node) = self.nodes.get(&src)
            && live
            && node.dirty
            && let Some(desired) = node.desired
            && node.held_epoch == Some(wire_epoch(desired.counter))
        {
            self.reissue(src);
        }
        // The node's 100 ms window is the only time in its sweep it listens. `send_admin` checks
        // the heartbeat is live.
        self.send_admin(src, now, batch);
        self.send_dedup_ring_clear(src, now, batch);
    }

    /// A batch of sightings, recorded unless it retransmits the batch before it.
    fn on_sightings(
        &mut self,
        rx: &Rx<'_>,
        sightings: SightingBatch<'_>,
        now: Now,
        batch: &mut ActionBatch,
    ) {
        let Rx { src, rssi, rx_us, payload, .. } = *rx;
        // Once per frame, not once per sighting.
        self.see_node(src, now, rssi, None, batch);
        if self.is_duplicate_batch(src, sightings.seq, payload, rx_us) {
            // A radio retry after a lost MAC ack. `see_node` already took it as proof of life.
            // Recording it again would double the observations.
            self.counters.duplicate_batches += 1;
            if let Some(node) = self.nodes.get_mut(&src) {
                node.duplicate_batches += 1;
            }
        } else {
            self.note_batch_seq(src, sightings.seq, now, batch);
            for (sighting, raw) in sightings.iter() {
                self.on_sighting(rx, sighting, raw, now, batch);
            }
        }
        self.remember_batch(src, payload, rx_us);
    }

    /// One record of a [`Frame::Sightings`] batch, after [`Self::see_node`] and
    /// [`Self::note_batch_seq`] ran for its frame.
    fn on_sighting(
        &mut self,
        rx: &Rx<'_>,
        sighting: SightingMsg<'_>,
        raw: &[u8],
        now: Now,
        batch: &mut ActionBatch,
    ) {
        let Rx { src, rssi, .. } = *rx;
        self.counters.observations += 1;
        match sighting.kind {
            RecordKind::Wifi => {
                self.unique_wifi.insert(&sighting.bssid);
            }
            RecordKind::Ble => {
                self.unique_ble.insert(&sighting.bssid);
            }
        }
        if let Some(node) = self.nodes.get_mut(&src) {
            node.observations += 1;
        }
        // The trailer's meaning depends on the kind: roaming consortium body for Wi-Fi, company
        // identifier for BLE. A BLE trailer that is not two bytes cannot come from a well-formed
        // frame of this wire version, so it is dropped rather than guessed at.
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
            link_rssi: Some(rssi),
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
            // `--record-raw` keeps the whole frame in `Record::Raw`.
            raw_body: raw.to_vec(),
        };
        self.push_tail(&observation);
        batch.records.push(Record::Observation(observation));
    }

    /// Track batches lost between this node and the host.
    ///
    /// `seq` counts up once per batch the bridge acknowledges, wraps, and restarts at boot
    /// ([`Self::on_heartbeat`] resets the baseline). A retransmission never reaches here
    /// ([`Self::is_duplicate_batch`]). A repeated `seq` that does is a send that failed after all
    /// retries though the frame arrived. The node reuses that `seq` for its next batch, so no loss
    /// is counted.
    ///
    /// A gap under [`UNCOUNTED_GAP`] is that many batches lost. At or past it the count wrapped or
    /// the frame came out of order, and guessing would invent history. Each counted gap is also a
    /// [`Record::BatchGap`], so the store's sum matches the live count.
    ///
    /// Only a gap after a live batch counts. One after a replayed batch spans time with no host
    /// reading, which is the bridge's `dropped_tx`. The few batches lost at the replay-to-live
    /// handoff go uncounted.
    fn note_batch_seq(&mut self, src: Mac, seq: u16, now: Now, batch: &mut ActionBatch) {
        // Read before borrowing the node. See `air_is_live`.
        let live = self.air_is_live();
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
        rssi: i8,
        capabilities: Option<Capabilities>,
        batch: &mut ActionBatch,
    ) {
        let node = self.nodes.entry(src).or_insert_with(|| NodeState::new(src, now));
        node.last_seen = now.mono;
        node.last_seen_ms = now.unix_ms;
        node.link_rssi = Some(rssi);
        batch.records.push(Record::Node(NodeSeen {
            mac: src,
            first_seen_ms: node.first_seen_ms,
            last_seen_ms: now.unix_ms,
            capabilities: capabilities.map(|caps| caps.to_string()),
        }));
    }
}
