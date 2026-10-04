use wartui_proto::air::{
    Capabilities, DecodeError, Frame, HeartbeatMsg, RecordKind, SightingBatch, SightingMsg,
    foreign, wire_epoch,
};
use wartui_proto::mac::Mac;
use wartui_proto::plan;

use super::{ActionBatch, FleetEngine, NodeState, Now};
use crate::record::{BatchGap, Heartbeat, NodeSeen, Observation, RawFrame, Record};

/// How long after a batch a byte-identical repeat under the same `seq` is still
/// an 802.11 retry rather than a node's own later re-send.
///
/// 100 ms, checked with a strict `<` so a repeat at or past it is never counted
/// as the retry. The radio's own retries arrived about 4 ms after the
/// original, every one of them within 100 ms (`docs/batch-loss-findings.md`).
/// A byte-identical same-`seq` batch that is not a retry is a node re-sending
/// addresses it has heard again, which takes at least a whole sweep: a sweep
/// has no admin window in it, so the shortest time between two reports of one
/// channel is a single dwell (`plan::CHANNEL_DWELL_MS`), and a one-channel
/// share re-reports every ~130 ms. A bridge reboot resets `rx_us`, so the
/// wrapped difference against a pre-reboot stamp is huge and the batch is
/// recorded rather than dropped — the safe direction.
pub(super) const DUPLICATE_BATCH_WINDOW_US: u64 = 100_000;

const _: () = assert!(
    DUPLICATE_BATCH_WINDOW_US < plan::CHANNEL_DWELL_MS as u64 * 1_000,
    "the shortest time between two reports of one channel is one dwell"
);

/// A since-boot count's contribution: nothing for a baseline, the whole value after a
/// restart or a fall, the difference otherwise.
///
/// A since-boot count only falls when the device restarts, so a fall is read as a restart
/// even when `restarted` misses it. A genuine u16 wrap reads the same way and undercounts by
/// at most one heartbeat's worth once every 65536, where a difference would add ~65000.
pub(crate) fn advance_since_boot(value: u64, prev: Option<u64>, restarted: bool) -> u64 {
    match prev {
        None => 0,
        Some(prev) if restarted || value < prev => value,
        Some(prev) => value - prev,
    }
}

impl FleetEngine {
    #[allow(
        clippy::too_many_arguments,
        reason = "the fields of one Rx frame, destructured at the call site"
    )]
    pub(super) fn on_rx(
        &mut self,
        src: Mac,
        dst: Mac,
        rssi: i8,
        rx_us: u32,
        payload: &[u8],
        now: Now,
        batch: &mut ActionBatch,
    ) {
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
            // Not ours at all (doesn't match our frame preamble).
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

        // Decode before admitting anyone to the fleet
        match frame {
            // Nothing to do about it, but an operator chasing a fleet that keeps
            // changing its mind needs to know. A clear this host did not send is
            // the same fact as an assignment this host did not send.
            Frame::Admin(_) | Frame::Clear(_) => self.counters.foreign_admin += 1,
            Frame::Heartbeat(heartbeat) => {
                self.on_heartbeat(src, rssi, rx_us, heartbeat, now, batch);
            }
            Frame::Sightings(sightings) => {
                self.on_sightings(src, rssi, rx_us, payload, sightings, now, batch);
            }
        }
    }

    fn on_heartbeat(
        &mut self,
        src: Mac,
        rssi: i8,
        rx_us: u32,
        heartbeat: HeartbeatMsg,
        now: Now,
        batch: &mut ActionBatch,
    ) {
        self.see_node(src, now, rssi, Some(heartbeat.capabilities), batch);
        self.counters.heartbeats += 1;
        // Read before the node is borrowed: whether this heartbeat is
        // live decides whether its epoch is believed at all. See
        // `air_is_live`.
        let live = self.air_is_live();
        let node = self.nodes.entry(src).or_insert_with(|| NodeState::new(src, now));
        // A counter below the last one means the node
        // restarted and has forgotten whatever range it was assigned.
        let counter_rebooted = node.counter.is_some_and(|previous| heartbeat.counter < previous);
        // A live heartbeat reporting no epoch after one previously reported
        // holding a real one is the same fact by a different route: the
        // node's epoch field went back to a boot value too.
        let epoch_rebooted =
            live && heartbeat.epoch == 0 && node.held_epoch.is_some_and(|previous| previous != 0);
        let rebooted = counter_rebooted || epoch_rebooted;
        if rebooted {
            node.reboots += 1;
            // The assignment is re-issued under a fresh epoch rather than
            // one the node might now match.
            node.confirmed = None;
            // A node that has just booted already holds an empty ring.
            node.clear_dedup_ring = false;
            // Its batch counter went back to a boot value as well, so the
            // gap between whatever it last sent and its first batch since
            // is a reboot's worth of history rather than a loss. The stored
            // bytes go with it: an identical batch after a reboot is a new
            // observation, not a retransmission of what was sent before.
            node.last_seq = None;
            node.last_seq_live = false;
            node.last_batch = None;
            node.last_batch_rx_us = None;
            // The node's epoch went back to a boot value too; what it
            // held is no longer known.
            node.held_epoch = None;
        }
        node.capabilities = Some(heartbeat.capabilities);
        // A replayed heartbeat's epoch is history, not now: it must not
        // overwrite what a live one already said.
        if live {
            node.held_epoch = Some(heartbeat.epoch);
        }
        // The node's since-boot counts, turned into this session's. A
        // node's first heartbeat is only a baseline, the same rule as the
        // bridge's `dropped_baseline`: what it refused before this capture
        // is not this capture's. A reboot restarted the count at 0, so the
        // whole value is new. A falling count is read as a restart too: a
        // node that reboots out of range can come back with a higher
        // counter. That only decides how drops are counted, not `reboots`.
        let advance = |value: u16, prev: Option<u16>| {
            advance_since_boot(u64::from(value), prev.map(u64::from), rebooted)
        };
        self.counters.wifi_dropped += advance(heartbeat.wifi_dropped, node.last_wifi_dropped);
        self.counters.ble_dropped += advance(heartbeat.ble_dropped, node.last_ble_dropped);
        node.last_wifi_dropped = Some(heartbeat.wifi_dropped);
        node.last_ble_dropped = Some(heartbeat.ble_dropped);
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
            wifi_dropped: heartbeat.wifi_dropped,
            ble_dropped: heartbeat.ble_dropped,
            beat: heartbeat.beat,
            live,
        }));

        if rebooted && node.desired.is_some() {
            self.reissue(src);
        }
        // Acked but not adopted: the node's radio delivered the frame, but
        // its heartbeat says it does not hold what it acknowledged — a
        // length or version mismatch, a dropped receive queue, or a bug.
        // `confirmed` is already `None` when this heartbeat is the reboot
        // itself, which is what keeps that case out of this one. Re-sent on
        // this window, the same retry path an unacknowledged assignment
        // takes; the epoch does not change, since the node still does not
        // hold it either way.
        //
        // Re-fetched rather than reused: `reissue` above needed the whole
        // of `self`, which the borrow checker will not let this hold across.
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
        // A node's first heartbeat is the moment it joins the fleet, and
        // every other node's range depends on how many there are.
        // Re-partitioning here rather than on the next tick is what lets
        // it take its share inside the window it has just opened.
        self.replan(now);
        // `deal` can allocate a counter before `held_epoch` is known (a backlog
        // replay leaves it `None`), and an ack can be lost after a node genuinely
        // adopted. Both look the same here: the node's heartbeat reports holding
        // exactly what `desired` carries. Re-issuing spends one more epoch and is
        // correct either way.
        if let Some(node) = self.nodes.get(&src)
            && live
            && node.dirty
            && let Some(desired) = node.desired
            && node.held_epoch == Some(wire_epoch(desired.counter))
        {
            self.reissue(src);
        }
        // The node holds its window open for 100 ms and its radio is gone
        // after that, so this is the only moment in the sweep worth
        // transmitting in — as long as the heartbeat is news.
        // `send_admin` checks that for itself.
        self.send_admin(src, now, batch);
        self.send_dedup_ring_clear(src, now, batch);
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the fields of one Rx frame, destructured at the call site"
    )]
    fn on_sightings(
        &mut self,
        src: Mac,
        rssi: i8,
        rx_us: u32,
        payload: &[u8],
        sightings: SightingBatch<'_>,
        now: Now,
        batch: &mut ActionBatch,
    ) {
        // Once for the whole frame: a batch is one node speaking once,
        // not `count` nodes speaking once each.
        self.see_node(src, now, rssi, None, batch);
        if self.is_duplicate_batch(src, sightings.seq, payload, rx_us) {
            // The node's radio retransmitted after a lost MAC ack, and
            // the bridge delivered both copies. `see_node` above already
            // recorded this as proof of life with a fresh link RSSI;
            // there is nothing else to record, since recording it again
            // would double the observations for one thing that happened
            // once.
            self.counters.duplicate_batches += 1;
            if let Some(node) = self.nodes.get_mut(&src) {
                node.duplicate_batches += 1;
            }
        } else {
            self.note_batch_seq(src, sightings.seq, now, batch);
            for (sighting, raw) in sightings.iter() {
                self.on_sighting(src, now, rssi, sighting, raw, batch);
            }
        }
        self.remember_batch(src, payload, rx_us);
    }

    /// One record out of a [`Frame::Sightings`] batch, after [`Self::see_node`]
    /// and [`Self::note_batch_seq`] have already run for the frame it came in.
    #[allow(clippy::too_many_arguments, reason = "the pieces of one record, at the call site")]
    fn on_sighting(
        &mut self,
        src: Mac,
        now: Now,
        rssi: i8,
        sighting: SightingMsg<'_>,
        raw: &[u8],
        batch: &mut ActionBatch,
    ) {
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
        // The trailer's meaning is the kind's — the roaming consortium
        // body for Wi-Fi, the company identifier for BLE — and this is
        // where that split is made. A trailer a well-formed frame of
        // this wire version cannot carry, a BLE one that is not exactly
        // two bytes, is dropped rather than guessed at.
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
            // The record's own bytes, not the batch around it: a batch's
            // shared header carries nothing a re-parse of one record needs,
            // and the whole frame is still kept in `Record::Raw` when
            // `--record-raw` is on.
            raw_body: raw.to_vec(),
        };
        self.push_tail(&observation);
        batch.records.push(Record::Observation(observation));
    }

    /// Track batches lost between this node and the host.
    ///
    /// `seq` counts up once per batch a node sends and the bridge acknowledges,
    /// wrapping and restarting at boot — the reboot arm above resets the
    /// baseline for the same reason it resets everything else a boot forgets.
    /// A MAC-layer retransmission — same `seq`, byte-identical, within
    /// [`DUPLICATE_BATCH_WINDOW_US`] — never reaches here: [`Self::is_duplicate_batch`]
    /// catches it in the `Sightings` arm first. A repeat that does reach here is a
    /// node whose send failed after all its retries even though the frame arrived:
    /// it never advanced `seq`, and its next batch — new content, or the same
    /// addresses heard again outside [`DUPLICATE_BATCH_WINDOW_US`] —
    /// reuses the number, so this counts no loss for it either.
    /// A gap under 1024 is batches lost in a row; at or past it, the count has
    /// wrapped or the frame arrived out of order, and guessing at a loss that
    /// large would invent history rather than report it. Each gap it counts is also
    /// recorded as a [`Record::BatchGap`], so the store's sum matches the live count.
    ///
    /// A gap counts only when the batch before it arrived live. A gap after a
    /// batch replayed from the bridge's backlog spans time when no host was
    /// reading, and what went missing then is the bridge's `dropped_tx`, not
    /// this count. The few batches lost at the replay-to-live handoff go
    /// uncounted.
    fn note_batch_seq(&mut self, src: Mac, seq: u16, now: Now, batch: &mut ActionBatch) {
        // Read before the node is borrowed. See `air_is_live`.
        let live = self.air_is_live();
        let Some(node) = self.nodes.get_mut(&src) else { return };
        if let Some(last) = node.last_seq.filter(|_| node.last_seq_live) {
            let gap = seq.wrapping_sub(last.wrapping_add(1));
            if gap < 1024 {
                node.batches_lost += u64::from(gap);
                self.counters.batches_lost += u64::from(gap);
            }
            if gap > 0 && gap < 1024 {
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

    /// Whether `payload` arriving at `rx_us` is a MAC-layer retransmission of
    /// this node's most recent batch: the same `seq`, byte-identical to what
    /// was stored for it, and received within [`DUPLICATE_BATCH_WINDOW_US`]
    /// of it.
    ///
    /// Same `seq` and same bytes also arises when the bridge received an
    /// earlier batch but every ack back was lost, so the node marked the send
    /// failed and left `seq` where it was; the same addresses, heard at the
    /// same RSSI, are then re-sent under that `seq` the next time it reports.
    /// The window is what tells the two apart — dropping a batch outside it
    /// would discard a genuine later sighting rather than a retry. A
    /// same-`seq` batch with different bytes is a different event entirely —
    /// see [`Self::note_batch_seq`] — and is not caught here.
    fn is_duplicate_batch(&self, src: Mac, seq: u16, payload: &[u8], rx_us: u32) -> bool {
        self.nodes.get(&src).is_some_and(|node| {
            node.last_seq == Some(seq)
                && node.last_batch.as_deref() == Some(payload)
                && node.last_batch_rx_us.is_some_and(|last_rx_us| {
                    u64::from(rx_us.wrapping_sub(last_rx_us)) < DUPLICATE_BATCH_WINDOW_US
                })
        })
    }

    /// Remember `payload` and `rx_us` as this node's most recent batch, for
    /// the next [`Self::is_duplicate_batch`] check. Called for every batch,
    /// duplicate or not, so the stored bytes and timestamp always match the
    /// last one seen.
    fn remember_batch(&mut self, src: Mac, payload: &[u8], rx_us: u32) {
        if let Some(node) = self.nodes.get_mut(&src) {
            node.last_batch = Some(payload.to_vec());
            node.last_batch_rx_us = Some(rx_us);
        }
    }

    /// Refresh what is known about the node at `src`, and file the row that
    /// says it was here.
    ///
    /// Split out because a heartbeat and a sighting both prove a node exists but
    /// only one says what it is: an observation's payload is a network, and
    /// filing that as an identity would name a node after something it heard.
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
