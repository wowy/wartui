//! One subscription to Up, from the first notification to the disconnect.
//!
//! The P4 clears its host queue when a central subscribes, announces `Ready` first and restarts
//! the generator's seq at 0. So an attachment whose first record is not `Ready`, or whose first
//! `SpikeLoad` is not seq 0, read a backlog from before it attached. Within one, a seq that skips
//! is records lost between the generator and here: evicted from the P4's host rings, refused by
//! them, or lost on the way. Notifications split frames anywhere, so bytes go through a
//! [`FrameAccumulator`] exactly as a serial port's do.

use std::collections::BTreeMap;
use std::time::Instant;

use serde::Serialize;
use wartui_proto::bus::{Heartbeat, NodeBoard, UpType};
use wartui_proto::fleet::Report;
use wartui_proto::link::{BridgeToHost, FrameAccumulator, MAX_FRAME, decode_frame};
use wartui_proto::spike::{SpikeStats, body_checks};

use crate::batches::{Batches, SlotBatches};
use crate::beats::{Beats, SlotBeats};
use crate::bridge::{self, Counters, FleetSummary};

/// Something `main` acts on as it decodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen {
    /// A `Status`, which answers a `GetStatus`.
    Status,
    /// A `Log` or an `Error`, as a line to print.
    Message(String),
}

/// A run of seqs that never arrived, both ends included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SeqGap {
    /// The first missing seq.
    pub from: u32,
    /// The last missing seq.
    pub to: u32,
}

/// The generator's seq and CRC, checked record by record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SeqCheck {
    /// `SpikeLoad` records that decoded.
    pub records: u64,
    /// The first seq seen.
    pub first: Option<u32>,
    /// The highest seq seen.
    pub last: Option<u32>,
    /// Seqs skipped.
    pub missing: u64,
    /// Where they were skipped.
    pub gaps: Vec<SeqGap>,
    /// Records whose seq was at or below one already seen.
    pub backwards: u64,
    /// Records whose body failed its CRC.
    pub crc_failures: u64,
}

impl SeqCheck {
    fn take(&mut self, seq: u32, body: &[u8]) {
        self.records += 1;
        if !body_checks(body) {
            self.crc_failures += 1;
        }
        self.first.get_or_insert(seq);
        if let Some(last) = self.last {
            let next = last.wrapping_add(1);
            if seq > next {
                self.gaps.push(SeqGap { from: next, to: seq - 1 });
                self.missing += u64::from(seq - next);
            } else if seq < next {
                // A repeat or a straggler: the high-water mark stands.
                self.backwards += 1;
                return;
            }
        }
        self.last = Some(seq);
    }
}

/// Throughput over a stretch, per second.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Rates {
    /// Seconds the figures cover.
    pub secs: f64,
    /// Up bytes, as notified.
    pub bytes_per_s: f64,
    /// Frames that decoded.
    pub frames_per_s: f64,
}

impl Rates {
    fn over(secs: f64, bytes: u64, frames: u64) -> Self {
        let per_s = |n: u64| if secs > 0.0 { n as f64 / secs } else { 0.0 };
        Self { secs, bytes_per_s: per_s(bytes), frames_per_s: per_s(frames) }
    }
}

/// One attachment's checks.
#[derive(Debug)]
pub struct Attachment {
    acc: FrameAccumulator<MAX_FRAME>,
    started: Instant,
    bytes: u64,
    frames: u64,
    window: (Instant, u64, u64),
    by_variant: BTreeMap<&'static str, u64>,
    garbled: u64,
    first: Option<&'static str>,
    board: Option<NodeBoard>,
    seq: SeqCheck,
    first_stats: Option<SpikeStats>,
    last_stats: Option<SpikeStats>,
    beats: Beats,
    batches: Batches,
    first_status: Option<Counters>,
    last_status: Option<Counters>,
    fleet: Option<Report>,
}

impl Attachment {
    /// An attachment that started at `now`.
    #[must_use]
    pub fn new(now: Instant) -> Self {
        Self {
            acc: FrameAccumulator::new(),
            started: now,
            bytes: 0,
            frames: 0,
            window: (now, 0, 0),
            by_variant: BTreeMap::new(),
            garbled: 0,
            first: None,
            board: None,
            seq: SeqCheck::default(),
            first_stats: None,
            last_stats: None,
            beats: Beats::default(),
            batches: Batches::default(),
            first_status: None,
            last_status: None,
            fleet: None,
        }
    }

    /// Take one notification's bytes.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Seen> {
        self.bytes += chunk.len() as u64;
        let mut seen = Vec::new();
        for &byte in chunk {
            // A frame that overran the buffer yields nothing at its terminator; count it here.
            if byte == 0 && self.acc.overflowed() {
                self.garbled += 1;
            }
            let Some(frame) = self.acc.push(byte) else { continue };
            match decode_frame::<BridgeToHost>(frame) {
                Ok(msg) => self.take(msg, &mut seen),
                Err(_) => self.garbled += 1,
            }
        }
        seen
    }

    fn take(&mut self, msg: BridgeToHost, seen: &mut Vec<Seen>) {
        let name = variant(&msg);
        self.frames += 1;
        *self.by_variant.entry(name).or_default() += 1;
        self.first.get_or_insert(name);
        match msg {
            BridgeToHost::Ready { node_board, .. } => self.board = Some(node_board),
            BridgeToHost::Status { .. } => {
                let counters = bridge::counters(&msg);
                if self.first_status.is_none() {
                    self.first_status.clone_from(&counters);
                }
                self.last_status = counters;
                seen.push(Seen::Status);
            }
            BridgeToHost::Fleet(report) => self.fleet = Some(report),
            BridgeToHost::Log { message } => seen.push(Seen::Message(format!("# log: {message}"))),
            BridgeToHost::Error { message } => {
                seen.push(Seen::Message(format!("# error: {message}")));
            }
            BridgeToHost::SpikeLoad { seq, body } => self.seq.take(seq, &body),
            BridgeToHost::SpikeStats(stats) => {
                self.first_stats.get_or_insert(stats);
                self.last_stats = Some(stats);
            }
            BridgeToHost::FromNode { slot, rx_us, kind, body } => match UpType::from_u8(kind) {
                Some(UpType::Heartbeat) => {
                    self.beats.take(slot, rx_us, &body);
                    if let Ok(heartbeat) = Heartbeat::decode(&body) {
                        let share = self
                            .fleet
                            .as_ref()
                            .and_then(|r| r.shares.iter().find(|s| s.slot == slot))
                            .map(|s| s.epoch);
                        self.batches.heartbeat(slot, &heartbeat, share);
                    }
                }
                Some(UpType::Sightings) => self.batches.take(slot, rx_us, &body),
                _ => {}
            },
            _ => {}
        }
    }

    /// The rates since the last call, or since the start, then a new window from `now`.
    pub fn window(&mut self, now: Instant) -> Rates {
        let (since, bytes, frames) = self.window;
        self.window = (now, self.bytes, self.frames);
        Rates::over(
            now.saturating_duration_since(since).as_secs_f64(),
            self.bytes - bytes,
            self.frames - frames,
        )
    }

    /// The rates over the whole attachment up to `now`.
    #[must_use]
    pub fn overall(&self, now: Instant) -> Rates {
        Rates::over(
            now.saturating_duration_since(self.started).as_secs_f64(),
            self.bytes,
            self.frames,
        )
    }

    /// Whether the first record that decoded was `Ready`.
    #[must_use]
    pub fn ready_first(&self) -> bool {
        self.first == Some("Ready")
    }

    /// The generator's checks so far.
    #[must_use]
    pub const fn seq(&self) -> &SeqCheck {
        &self.seq
    }

    /// Frames that failed to decode or overran the buffer.
    #[must_use]
    pub const fn garbled(&self) -> u64 {
        self.garbled
    }

    /// Frames that decoded, of the variant named.
    #[must_use]
    pub fn count(&self, variant: &str) -> u64 {
        self.by_variant.get(variant).copied().unwrap_or(0)
    }

    /// The first and the latest `SpikeStats`.
    #[must_use]
    pub const fn stats(&self) -> (Option<SpikeStats>, Option<SpikeStats>) {
        (self.first_stats, self.last_stats)
    }

    /// The heartbeats seen.
    #[must_use]
    pub const fn beats(&self) -> &Beats {
        &self.beats
    }

    /// The sighting batches seen.
    #[must_use]
    pub const fn batches(&self) -> &Batches {
        &self.batches
    }

    /// Everything checked, up to `now`.
    #[must_use]
    pub fn summary(&self, now: Instant) -> AttachmentSummary {
        let board = self.board.unwrap_or(NodeBoard::NoCarrier);
        AttachmentSummary {
            rates: self.overall(now),
            bytes: self.bytes,
            frames: self.frames,
            by_variant: self.by_variant.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect(),
            garbled: self.garbled,
            first_record: self.first.map(str::to_owned),
            ready_first: self.ready_first(),
            seq_from_zero: self.seq.first.is_none_or(|first| first == 0),
            spike: self.seq.clone(),
            first_stats: self.first_stats,
            last_stats: self.last_stats,
            beats: self.beats.labeled(board),
            batches: self.batches.labeled(board),
            first_status: self.first_status.clone(),
            last_status: self.last_status.clone(),
            fleet: self.fleet.as_ref().map(|r| FleetSummary::of(r, board)),
        }
    }
}

/// What one attachment came to, for the summary and its JSON.
#[derive(Debug, Clone, Serialize)]
pub struct AttachmentSummary {
    /// Throughput over the whole attachment.
    pub rates: Rates,
    /// Up bytes, as notified.
    pub bytes: u64,
    /// Frames that decoded.
    pub frames: u64,
    /// Those frames by variant.
    pub by_variant: BTreeMap<String, u64>,
    /// Frames that failed to decode or overran the buffer.
    pub garbled: u64,
    /// The first record that decoded.
    pub first_record: Option<String>,
    /// Whether it was `Ready`.
    pub ready_first: bool,
    /// Whether the first `SpikeLoad` was seq 0, or none came.
    pub seq_from_zero: bool,
    /// The generator's checks.
    pub spike: SeqCheck,
    /// The first `SpikeStats`.
    pub first_stats: Option<SpikeStats>,
    /// The latest `SpikeStats`.
    pub last_stats: Option<SpikeStats>,
    /// Heartbeats per slot, by label.
    pub beats: BTreeMap<String, SlotBeats>,
    /// Sighting batches per slot, by label.
    pub batches: BTreeMap<String, SlotBatches>,
    /// The first `Status`'s counters.
    pub first_status: Option<Counters>,
    /// The latest `Status`'s counters.
    pub last_status: Option<Counters>,
    /// The latest `Fleet` report.
    pub fleet: Option<FleetSummary>,
}

/// The variant's name, for counting.
#[must_use]
pub const fn variant(msg: &BridgeToHost) -> &'static str {
    match msg {
        BridgeToHost::Ready { .. } => "Ready",
        BridgeToHost::Status { .. } => "Status",
        BridgeToHost::Log { .. } => "Log",
        BridgeToHost::Error { .. } => "Error",
        BridgeToHost::Gps { .. } => "Gps",
        BridgeToHost::Update(_) => "Update",
        BridgeToHost::FromNode { .. } => "FromNode",
        BridgeToHost::Delivered { .. } => "Delivered",
        BridgeToHost::Undelivered { .. } => "Undelivered",
        BridgeToHost::PowerCycled { .. } => "PowerCycled",
        BridgeToHost::CacheImage(_) => "CacheImage",
        BridgeToHost::Fleet(_) => "Fleet",
        BridgeToHost::SpikeLoad { .. } => "SpikeLoad",
        BridgeToHost::SpikeStats(_) => "SpikeStats",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use wartui_proto::link::{LoopPhase, ShortStr, SpikeBody, encode_frame};
    use wartui_proto::reset::ResetCause;
    use wartui_proto::spike::{SMALL_BODY, fill_body};

    fn frame(msg: &BridgeToHost) -> Vec<u8> {
        let mut out = vec![0u8; MAX_FRAME];
        let len = encode_frame(msg, &mut out).unwrap();
        out.truncate(len);
        out
    }

    fn ready() -> BridgeToHost {
        BridgeToHost::Ready {
            mac: [0x10, 0x20, 0x30, 0x40, 0x96, 0xC4],
            fw_version: ShortStr::try_from("spike").unwrap(),
            reset_cause: ResetCause::PowerOn,
            last_phase: LoopPhase::Unknown,
            uptime_ms: 1,
            node_board: NodeBoard::Carrier10,
            radio: None,
            image: None,
        }
    }

    fn load(seq: u32) -> BridgeToHost {
        let mut body = SpikeBody::new();
        body.resize(SMALL_BODY, 0).unwrap();
        fill_body(seq, &mut body);
        BridgeToHost::SpikeLoad { seq, body }
    }

    /// The frames' bytes end to end, cut into notifications of `size`.
    fn stream(msgs: &[BridgeToHost]) -> Vec<u8> {
        msgs.iter().flat_map(frame).collect()
    }

    fn fed(bytes: &[u8], size: usize) -> Attachment {
        let mut attachment = Attachment::new(Instant::now());
        for chunk in bytes.chunks(size) {
            attachment.feed(chunk);
        }
        attachment
    }

    fn with_seqs(seqs: impl IntoIterator<Item = u32>) -> Vec<BridgeToHost> {
        std::iter::once(ready()).chain(seqs.into_iter().map(load)).collect()
    }

    #[test]
    fn standin_counts_no_gap_when_every_record_arrives() {
        let bytes = stream(&with_seqs(0..50));
        for size in [1, 7, 20, 244, 495, bytes.len()] {
            let attachment = fed(&bytes, size);
            let seq = attachment.seq();
            assert_eq!(seq.records, 50, "chunks of {size}");
            assert_eq!((seq.missing, seq.backwards, seq.crc_failures), (0, 0, 0));
            assert!(attachment.ready_first());
            assert_eq!(attachment.garbled(), 0);
            assert_eq!(attachment.count("SpikeLoad"), 50);
        }
    }

    #[test]
    fn standin_counts_seq_gap_when_record_missing() {
        let seqs = (0..10).chain(13..20).chain([25]);
        let attachment = fed(&stream(&with_seqs(seqs)), 33);
        let seq = attachment.seq();
        assert_eq!(seq.missing, 3 + 5);
        assert_eq!(seq.gaps, [SeqGap { from: 10, to: 12 }, SeqGap { from: 20, to: 24 }]);
        assert_eq!(seq.last, Some(25));
    }

    #[test]
    fn standin_counts_backwards_when_seq_repeats() {
        let attachment = fed(&stream(&with_seqs([0, 1, 2, 2, 1, 3])), 64);
        let seq = attachment.seq();
        assert_eq!((seq.backwards, seq.missing), (2, 0));
    }

    #[test]
    fn standin_counts_crc_failure_when_body_damaged() {
        let mut bad = load(1);
        if let BridgeToHost::SpikeLoad { body, .. } = &mut bad {
            body[4] ^= 0xFF;
        }
        let attachment = fed(&stream(&[ready(), load(0), bad, load(2)]), 9);
        assert_eq!(attachment.seq().crc_failures, 1);
        assert_eq!(attachment.seq().missing, 0);
    }

    #[test]
    fn standin_counts_garbled_when_frame_corrupted() {
        let mut bytes = stream(&with_seqs(0..3));
        // Flip a byte inside seq 1's frame; the link CRC fails it.
        let at = frame(&ready()).len() + frame(&load(0)).len() + 5;
        bytes[at] ^= 0x01;
        let attachment = fed(&bytes, 20);
        assert_eq!(attachment.garbled(), 1);
        assert_eq!(attachment.seq().records, 2);
        assert_eq!(attachment.seq().gaps, [SeqGap { from: 1, to: 1 }]);
    }

    #[test]
    fn standin_counts_garbled_when_frame_overruns_buffer() {
        let mut bytes = vec![0x55; MAX_FRAME + 10];
        bytes.push(0);
        bytes.extend(stream(&with_seqs(0..2)));
        let attachment = fed(&bytes, 100);
        assert_eq!(attachment.garbled(), 1);
        assert_eq!(attachment.seq().records, 2);
    }

    #[test]
    fn standin_flags_backlog_when_ready_not_first() {
        let bytes = stream(&[load(7), ready(), load(8)]);
        let attachment = fed(&bytes, 50);
        assert!(!attachment.ready_first());
        let summary = attachment.summary(Instant::now());
        assert_eq!(summary.first_record.as_deref(), Some("SpikeLoad"));
        assert!(!summary.seq_from_zero);
    }

    #[test]
    fn standin_reports_status_and_messages_when_decoded() {
        let status = BridgeToHost::Status {
            rx_count: 0,
            dropped_tx: 0,
            uptime_ms: 0,
            rx_oversize: 0,
            queues: Default::default(),
            radio_link: Default::default(),
            host_uart_errors: 0,
            fan: None,
            pool: Default::default(),
            scan: Default::default(),
            ble: None,
            present: 0,
            members: Default::default(),
        };
        let log = BridgeToHost::Log { message: "hello".try_into().unwrap() };
        let mut attachment = Attachment::new(Instant::now());
        let seen = attachment.feed(&stream(&[ready(), status, log]));
        assert_eq!(seen, [Seen::Status, Seen::Message("# log: hello".to_owned())]);
    }

    #[test]
    fn standin_keeps_first_and_latest_stats_when_several_arrive() {
        let stats =
            |evicted| BridgeToHost::SpikeStats(SpikeStats { evicted, ..Default::default() });
        let attachment = fed(&stream(&[ready(), stats(3), stats(4), stats(9)]), 5);
        let (first, last) = attachment.stats();
        assert_eq!((first.unwrap().evicted, last.unwrap().evicted), (3, 9));
    }

    #[test]
    fn standin_tallies_heartbeats_when_from_node_carries_one() {
        use wartui_proto::bus::{Board, Capabilities, DieTemp, Heartbeat, Slot, Vdd};
        let slot = Slot::new(1, 2).unwrap();
        let beat = |rx_us, counter| {
            let body = Heartbeat {
                capabilities: Capabilities::here(Board::Esp32c5),
                mac: [1; 6],
                counter,
                epoch: 1,
                wifi_refused: 0,
                ble_refused: 0,
                reset_cause: ResetCause::PowerOn,
                vdd: Vdd::NONE,
                die_temp: DieTemp::NONE,
                heap_free: 0,
            }
            .encode();
            BridgeToHost::FromNode {
                slot,
                rx_us,
                kind: UpType::Heartbeat.as_u8(),
                body: body.as_slice().try_into().unwrap(),
            }
        };
        let attachment =
            fed(&stream(&[ready(), beat(0, 1), beat(5_000_000, 7), beat(20_000_000, 30)]), 13);
        let tally = attachment.beats().slot(slot).unwrap();
        assert_eq!((tally.beats, tally.missed), (3, 2));
        let summary = attachment.summary(Instant::now());
        // A ten-node carrier labels bus B's third slot N8.
        assert!(summary.beats.contains_key("N8"), "{:?}", summary.beats.keys());
    }

    #[test]
    fn standin_counts_lost_batches_when_from_node_sightings_skip_seq() {
        use wartui_proto::bus::{
            Board, Capabilities, DieTemp, Heartbeat, RecordKind, Security, SightingBatchWriter,
            SightingMsg, Slot, Vdd,
        };
        use wartui_proto::fleet::{Report, Share};
        use wartui_proto::plan::ChannelSet;
        let slot = Slot::new(0, 0).unwrap();
        let from_node = |rx_us, kind: UpType, body: &[u8]| BridgeToHost::FromNode {
            slot,
            rx_us,
            kind: kind.as_u8(),
            body: body.try_into().unwrap(),
        };
        let beat = |rx_us, counter, epoch| {
            let body = Heartbeat {
                capabilities: Capabilities::here(Board::Esp32c5),
                mac: [1; 6],
                counter,
                epoch,
                wifi_refused: 0,
                ble_refused: 0,
                reset_cause: ResetCause::PowerOn,
                vdd: Vdd::NONE,
                die_temp: DieTemp::NONE,
                heap_free: 0,
            }
            .encode();
            from_node(rx_us, UpType::Heartbeat, &body)
        };
        let sightings = |rx_us, seq| {
            let mut writer = SightingBatchWriter::new(seq);
            let sighting = SightingMsg {
                kind: RecordKind::Wifi,
                bssid: [2; 6],
                channel: 1,
                rssi: -50,
                security: Security::from_u8(0),
                ssid: b"x",
                ext: &[],
            };
            assert!(writer.push(&sighting));
            from_node(rx_us, UpType::Sightings, writer.as_bytes())
        };
        let share = Share { slot, epoch: 5, ble: false, channels: ChannelSet::empty() };
        let fleet = BridgeToHost::Fleet(Report {
            mode: Default::default(),
            pool: Default::default(),
            shares: [share].into_iter().collect(),
            unreachable: ChannelSet::empty(),
        });
        let msgs = [
            ready(),
            fleet,
            beat(0, 10, 5),
            sightings(1_000, 1),
            sightings(2_000, 2),
            sightings(3_000, 6),
            // Epoch 0 under the share it held, counter still climbing: a restart.
            beat(5_000_000, 12, 0),
            sightings(6_000_000, 0),
            sightings(7_000_000, 1),
        ];
        let attachment = fed(&stream(&msgs), 17);
        let tally = attachment.batches().slot(slot).unwrap();
        assert_eq!((tally.batches, tally.lost, tally.gaps, tally.restarts), (5, 3, 1, 1));
        let summary = attachment.summary(Instant::now());
        assert_eq!(summary.batches["N1"].lost, 3);
        assert_eq!(summary.fleet.unwrap().members[0].slot, "N1");
    }

    #[test]
    fn standin_keeps_first_and_latest_status_when_several_arrive() {
        let status = |rx_count| BridgeToHost::Status {
            rx_count,
            dropped_tx: 0,
            uptime_ms: 0,
            rx_oversize: 0,
            queues: Default::default(),
            radio_link: Default::default(),
            host_uart_errors: 0,
            fan: None,
            pool: Default::default(),
            scan: Default::default(),
            ble: None,
            present: 0,
            members: Default::default(),
        };
        let attachment = fed(&stream(&[ready(), status(3), status(4), status(9)]), 11);
        let summary = attachment.summary(Instant::now());
        let rx = |c: &Option<Counters>| {
            c.as_ref().unwrap().iter().find(|(n, _)| n == "rx_count").map(|(_, v)| *v)
        };
        assert_eq!((rx(&summary.first_status), rx(&summary.last_status)), (Some(3), Some(9)));
    }

    #[test]
    fn standin_measures_window_rates_when_asked_twice() {
        let start = Instant::now();
        let mut attachment = Attachment::new(start);
        let bytes = stream(&with_seqs(0..4));
        attachment.feed(&bytes);
        let first = attachment.window(start + Duration::from_secs(2));
        assert!((first.bytes_per_s - bytes.len() as f64 / 2.0).abs() < 1e-6);
        assert!((first.frames_per_s - 2.5).abs() < 1e-6);
        let second = attachment.window(start + Duration::from_secs(4));
        assert!(second.bytes_per_s.abs() < 1e-6);
        let overall = attachment.overall(start + Duration::from_secs(4));
        assert!((overall.frames_per_s - 1.25).abs() < 1e-6);
    }
}
