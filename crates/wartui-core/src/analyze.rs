//! What a capture lost on the way in, and how fast it arrived, read back from the tables the
//! store already writes.
//!
//! Heartbeats are counted per node, in arrival order (`id`, since the store has one writer), not
//! by `rx_at`, which the wall clock can step back. A heartbeat carries no sequence of its own, so
//! a lost one is not counted: the node bus does not resend a heartbeat, and a newer one replaces
//! one the bridge has not yet clocked. A restart is `counter` (u32 sweeps) falling by half its
//! range or more, which assumes fewer than 2^31 sweeps between two rows.
//!
//! Refusals are reported beside the losses, not as one. A full pending buffer turns a sighting away
//! once per dwell or scan, and the address is usually reported on a later one. The count is
//! buffer pressure, not missing sightings.
//!
//! On the bridge side, each `bridge_status` row carries `host_frames`, the frames the host had read
//! when the reply arrived. `host_read` is the frames read between the first reply and the last.
//! `received` less the bridge's drops less `host_read` is the frames lost between the bridge's
//! queue and the host. It is approximate: a status reply is a priority frame and overtakes the
//! frames queued in the bulk ring, which `rx_count` counts and `host_frames` does not yet. So it is
//! off by up to the ring's 24 frames at each end, and saturates at 0 when a baseline taken
//! mid-backlog shows the host reading more than the bridge passed on.
//!
//! Each status reply also carries the bridge's queues, one `bridge_queue` row each, and its radio
//! link's counters. A queue's `peak` is the largest `high_water` any row
//! carried. That figure is since the bridge booted, so the peak can predate the capture. Its drops
//! and the radio link counters are since-boot counts, rebased like the bridge's own: a restart
//! between two rows is the reply's uptime falling at any row between them. That uptime is the C6's.
//! The link's `packets`, `crc_fail`, `bad_magic`, `seq_gap` and `uart_errors` are the P4's and run on
//! through a C6 restart, so they rebase only when they fall, as `host_uart_errors` does, and the `radio_*` counters follow the
//! uptime. A P4 reboot whose counters regrow past the last row reads as no reboot, which is rare and
//! only undercounts. Queues follow the same split, by [`QueueId::on_p4`].
//!
//! `dropped_tx` sums P4 and C6 queues into one number, so neither rule fits it: a C6 restart lowers
//! it by only part of its total. A capture whose rows carry those queues takes `dropped` from their
//! own deltas ([`QueueId::in_dropped_tx`]). Only rows without queues rebase `dropped_tx` itself.
//!
//! On the host side, [`HostLoss`] breaks down what the host read and did not store, from the
//! `host_status` rows. Their counts run from the engine's start and are read as the last row less
//! the first, which is written as the capture starts. Peaks are per row, so it takes the largest,
//! and counts the rows whose lag peak reached the 100 ms that makes a heartbeat too stale to
//! answer.
//!
//! Every since-boot count is rebased by the engine's `advance_since_boot`, and the first row is a
//! baseline only (per node, for heartbeats): what was dropped before the capture is not the
//! capture's. Analyze infers restarts from `counter`. Live, the engine also reads a heartbeat's
//! epoch falling back to 0, so the two can disagree after a restart the counter misses.
//!
//! Arrival rate is [`peaks`]: the most sightings in any one second and any one minute. Windows
//! slide rather than fall on clock boundaries, since a burst astride a second is one burst to the
//! link. They are half-open, `(t - span, t]`, so a steady 1 Hz source reads 1 per second. Time is
//! the host's `rx_at`, so a backlog replayed after a reconnect counts as the burst it was on the
//! link. Rows are walked by `id`, like the heartbeats, and every window restarts where `rx_at`
//! steps back: sorting would merge two real intervals into one and invent a peak.
//!
//! Per-node figures name each node's slot, the one its latest heartbeat (by `id`) came from, and
//! list nodes in slot order, which on a carrier is `N1` up. A node that moved so appears once, and
//! two nodes end on one slot when one stopped heartbeating and another later heartbeated from it.
//! `heartbeat.slot` means something to the operator only through `capture.node_board`, so with no
//! carrier, or none known, no node has a slot. [`losses`] reads the slots with the heartbeats, and
//! [`peaks`] takes them from it.
//!
//! A node's rail reading has a per-chip offset, so `supply_drop` applies the engine's rule against
//! that node's first reading in the capture. The live warning follows the latest reading. This one
//! stays set once tripped, because the report summarizes the whole capture.

use std::collections::{BTreeMap, VecDeque};

use rusqlite::{Connection, OptionalExtension, Row};
use wartui_proto::bus::{NodeBoard, RecordKind, Slot};
use wartui_proto::link::QueueId;
use wartui_proto::mac::Mac;

use crate::engine::{BEHIND_US, advance_since_boot, supply_dropped_from};
use crate::store::parse_kind;

/// What a capture lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LossSummary {
    /// What carried the capture's nodes. No carrier for a capture whose bridge never announced
    /// itself or named a board this build does not know.
    pub board: NodeBoard,
    /// The bridge's own counts, or `None` when the capture holds no status reply.
    pub bridge: Option<BridgeLoss>,
    /// One entry per node that sent a heartbeat or lost a batch: by slot, then slotless nodes by
    /// address.
    pub nodes: Vec<NodeLoss>,
    /// The host's own figures, or `None` when the capture holds no `host_status` row.
    pub host: Option<HostLoss>,
}

impl Default for LossSummary {
    fn default() -> Self {
        Self { board: NodeBoard::NoCarrier, bridge: None, nodes: Vec::new(), host: None }
    }
}

impl LossSummary {
    /// Each node's slot, for [`peaks`].
    #[must_use]
    pub fn slots(&self) -> BTreeMap<Mac, Slot> {
        self.nodes.iter().filter_map(|node| Some((node.mac, node.slot?))).collect()
    }
}

/// What the bridge received and dropped during the capture. Exact but for `usb_lost`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BridgeLoss {
    /// Records the bridge received from its nodes.
    pub received: u64,
    /// Frames the bridge dropped on their way to the host: its queues' drops when rows carry them,
    /// else `dropped_tx`'s rise. The module documentation says why.
    pub dropped: u64,
    /// Times the bridge's uptime fell, which is a restart.
    pub reboots: u64,
    /// Frames the host read off the link between the first status reply and the last.
    pub host_read: u64,
    /// Frames lost between the bridge's queue and the host: `received` less `dropped` less
    /// `host_read`, saturating at 0. Approximate to the 24 frames the bulk ring can queue at each
    /// end, which a status reply overtakes.
    pub usb_lost: u64,
    /// Frames the bridge dropped for being longer than any node sends: somebody else's
    /// traffic, and not in `received`.
    pub oversize: u64,
    /// The bridge's queues, in the order it first reported them; empty when it reports none.
    pub queues: Vec<QueueLoss>,
    /// The P4–C6 UART link's counters.
    pub radio_link: RadioLinkLoss,
    /// The P4's host-link reads that saw one or more line errors or overflows: host bytes lost
    /// or corrupted on the way in.
    pub host_uart_errors: u64,
}

/// How full one of the bridge's queues got, and what it dropped during the capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueLoss {
    /// Which queue. Every figure is bytes.
    pub queue: QueueId,
    /// Its size, as the last reply gave it.
    pub capacity: u64,
    /// The most it held at once since the bridge booted, which can predate the capture.
    pub peak: u64,
    /// What it evicted or refused during the capture.
    pub dropped: u64,
}

/// The P4–C6 UART link's counters over the capture. Named as
/// [`RadioLinkStats`](wartui_proto::link::RadioLinkStats) names them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RadioLinkLoss {
    /// Verified C6→P4 packets.
    pub packets: u64,
    /// C6→P4 packets whose CRC failed.
    pub crc_fail: u64,
    /// C6→P4 packets with the wrong magic.
    pub bad_magic: u64,
    /// C6→P4 packets whose sequence number skipped.
    pub seq_gap: u64,
    /// Line errors on the P4's UART.
    pub uart_errors: u64,
    /// P4→C6 packets the C6 saw fail their CRC.
    pub radio_crc_fail: u64,
    /// P4→C6 packets the C6 saw with the wrong magic.
    pub radio_bad_magic: u64,
    /// P4→C6 packets the C6 saw skip a sequence number.
    pub radio_seq_gap: u64,
    /// Line errors on the C6's UART.
    pub radio_uart_errors: u64,
}

impl RadioLinkLoss {
    /// A status row's counters.
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        let get = |name: &str| row.get::<_, i64>(name).map(count);
        Ok(Self {
            packets: get("radio_link_packets")?,
            crc_fail: get("radio_link_crc_fail")?,
            bad_magic: get("radio_link_bad_magic")?,
            seq_gap: get("radio_link_seq_gap")?,
            uart_errors: get("radio_link_uart_errors")?,
            radio_crc_fail: get("radio_link_radio_crc_fail")?,
            radio_bad_magic: get("radio_link_radio_bad_magic")?,
            radio_seq_gap: get("radio_link_radio_seq_gap")?,
            radio_uart_errors: get("radio_link_radio_uart_errors")?,
        })
    }

    /// Adds what each counter advanced from `before` to `now`. `restarted` is the C6's uptime
    /// falling, which only the `radio_*` counters restart with; see the module documentation.
    fn advance(&mut self, now: &Self, before: &Self, restarted: bool) {
        let p4 = |now, before| advance_since_boot(now, Some(before), false);
        let c6 = |now, before| advance_since_boot(now, Some(before), restarted);
        self.packets += p4(now.packets, before.packets);
        self.crc_fail += p4(now.crc_fail, before.crc_fail);
        self.bad_magic += p4(now.bad_magic, before.bad_magic);
        self.seq_gap += p4(now.seq_gap, before.seq_gap);
        self.uart_errors += p4(now.uart_errors, before.uart_errors);
        self.radio_crc_fail += c6(now.radio_crc_fail, before.radio_crc_fail);
        self.radio_bad_magic += c6(now.radio_bad_magic, before.radio_bad_magic);
        self.radio_seq_gap += c6(now.radio_seq_gap, before.radio_seq_gap);
        self.radio_uart_errors += c6(now.radio_uart_errors, before.radio_uart_errors);
    }
}

/// What the host did with the frames it read, and how it held up, first `host_status` row to last.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostLoss {
    /// Sighting batches dropped as node-bus resends.
    pub duplicates: u64,
    /// USB frames that failed their checksum.
    pub garbled: u64,
    /// Frames of ours that would not parse.
    pub undecodable: u64,
    /// Frames from a build speaking another wire version or layout.
    pub incompatible: u64,
    /// Rows the store dropped.
    pub store_dropped: u64,
    /// Rows whose lag peak reached 100 ms: samples in which the host fell behind the bridge.
    pub lag_over_100ms: u64,
    /// The furthest behind the bridge the host fell, in microseconds.
    pub lag_peak_us: u64,
    /// The slowest batch the store wrote, in microseconds.
    pub commit_peak_us: u64,
    /// The deepest the store's queue got.
    pub queue_peak: u64,
    /// Rows reporting under-voltage now (bit 0), or `None` when no row read the Pi's
    /// throttle word.
    pub under_voltage: Option<u64>,
    /// Rows reporting a capped frequency, throttling or the soft temperature limit now
    /// (bits 1–3), or `None` as for `under_voltage`.
    pub throttled: Option<u64>,
    /// The last throttle word's since-boot bits (16–19), shifted to 0–3 like the "now" bits, so
    /// trouble from before the capture shows too. `None` as for `under_voltage`.
    pub since_boot: Option<u32>,
    /// The highest SoC temperature, in milli-degrees Celsius, or `None` when no row read one.
    pub temp_max_mc: Option<i32>,
    /// The lowest battery voltage, in millivolts, or `None` when no row read one.
    pub battery_min_mv: Option<i32>,
}

/// What one node lost during the capture.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeLoss {
    /// The node's address.
    pub mac: Mac,
    /// The slot its latest heartbeat came from. `None` with no heartbeat or no carrier.
    pub slot: Option<Slot>,
    /// Sighting batches lost between the node and the host. Exact, from `batch_gap`.
    pub batches_lost: u64,
    /// Heartbeats the capture holds.
    pub heartbeats: u64,
    /// Access points the node's pending buffer refused. Most are reported on a later dwell.
    pub wifi_refused: u64,
    /// Advertisers the node's pending buffer refused.
    pub ble_refused: u64,
    /// A rail reading fell far enough below the node's first in the capture to count as a drop,
    /// even if it later recovered.
    pub supply_drop: bool,
}

/// How fast sightings arrived.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeakSummary {
    /// Rows walked: every sighting the host stored, including those an export leaves out.
    pub sightings: u64,
    /// Across every node and kind.
    pub total: Peak,
    /// Wi-Fi sightings alone.
    pub wifi: Peak,
    /// BLE sightings alone. A kind this build does not know counts in `total` and its node only.
    pub ble: Peak,
    /// One entry per node that stored a sighting: by slot, then slotless nodes by address.
    pub nodes: Vec<NodePeak>,
    /// Times `rx_at` fell between consecutive rows. Every window restarts there, so a peak
    /// around a step is a lower bound.
    pub clock_reversals: u64,
}

/// The most sightings in one sliding window of each span.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Peak {
    /// In any one second: a sighting and one exactly 1,000 ms earlier share no window.
    pub per_second: u64,
    /// In any one minute, likewise half-open.
    pub per_minute: u64,
}

/// One node's peak arrival rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodePeak {
    /// The node's address.
    pub mac: Mac,
    /// The slot its latest heartbeat came from. `None` with no heartbeat or no carrier.
    pub slot: Option<Slot>,
    /// Its sightings' peaks.
    pub peak: Peak,
}

/// What the capture lost. Missed heartbeats follow the module's modulo assumptions;
/// the bridge's `usb_lost` is approximate.
pub fn losses(conn: &Connection) -> rusqlite::Result<LossSummary> {
    let board = node_board(conn)?;
    let bridge = bridge(conn)?;
    let mut nodes: BTreeMap<Mac, NodeLoss> = BTreeMap::new();
    heartbeats(conn, board, &mut nodes)?;
    batches(conn, &mut nodes)?;
    let mut nodes: Vec<NodeLoss> = nodes.into_values().collect();
    nodes.sort_by_key(|node| slot_order(node.slot, node.mac));
    Ok(LossSummary { board, bridge, nodes, host: host(conn)? })
}

/// What carried the capture's nodes, as [`LossSummary::board`] says.
fn node_board(conn: &Connection) -> rusqlite::Result<NodeBoard> {
    let name: Option<String> = conn
        .query_row("SELECT node_board FROM capture", [], |row| row.get(0))
        .optional()?
        .flatten();
    let known = name.and_then(|name| NodeBoard::ALL.into_iter().find(|b| b.name() == name));
    Ok(known.unwrap_or(NodeBoard::NoCarrier))
}

/// Sorts by slot, with slotless nodes after and ties by address.
fn slot_order(slot: Option<Slot>, mac: Mac) -> (bool, Option<Slot>, Mac) {
    (slot.is_none(), slot, mac)
}

/// The most sightings that came over the link in any one second and any one minute, from every
/// `observation` row, with each node's slot from `slots` ([`LossSummary::slots`]). The module doc
/// says how the windows are counted.
pub fn peaks(conn: &Connection, slots: &BTreeMap<Mac, Slot>) -> rusqlite::Result<PeakSummary> {
    let mut stmt = conn.prepare("SELECT rx_at, node_mac, kind FROM observation ORDER BY id")?;
    let mut rows = stmt.query([])?;
    let mut summary = PeakSummary::default();
    let (mut total, mut wifi, mut ble) = (Rate::new(), Rate::new(), Rate::new());
    let mut nodes: BTreeMap<Mac, Rate> = BTreeMap::new();
    let mut prev_at: Option<i64> = None;
    while let Some(row) = rows.next()? {
        let at_ms: i64 = row.get(0)?;
        let mac: Mac = row.get(1)?;
        let kind = parse_kind(row.get_ref(2)?.as_str()?);
        summary.sightings += 1;
        if prev_at.is_some_and(|prev| at_ms < prev) {
            summary.clock_reversals += 1;
            for rate in [&mut total, &mut wifi, &mut ble].into_iter().chain(nodes.values_mut()) {
                rate.clear();
            }
        }
        prev_at = Some(at_ms);
        total.push(at_ms);
        nodes.entry(mac).or_insert_with(Rate::new).push(at_ms);
        match kind {
            Some(RecordKind::Wifi) => wifi.push(at_ms),
            Some(RecordKind::Ble) => ble.push(at_ms),
            None => {}
        }
    }
    summary.total = total.peak();
    summary.wifi = wifi.peak();
    summary.ble = ble.peak();
    summary.nodes = nodes
        .into_iter()
        .map(|(mac, rate)| NodePeak { mac, slot: slots.get(&mac).copied(), peak: rate.peak() })
        .collect();
    summary.nodes.sort_by_key(|node| slot_order(node.slot, node.mac));
    Ok(summary)
}

/// A one-second and a one-minute window over the same sightings.
struct Rate {
    second: Sliding,
    minute: Sliding,
}

impl Rate {
    fn new() -> Self {
        Self { second: Sliding::new(1_000), minute: Sliding::new(60_000) }
    }

    fn push(&mut self, at_ms: i64) {
        self.second.push(at_ms);
        self.minute.push(at_ms);
    }

    fn clear(&mut self) {
        self.second.clear();
        self.minute.clear();
    }

    fn peak(&self) -> Peak {
        Peak { per_second: self.second.peak, per_minute: self.minute.peak }
    }
}

/// The arrivals within `span_ms` of the latest, and the most it has held.
struct Sliding {
    span_ms: i64,
    buf: VecDeque<i64>,
    peak: u64,
}

impl Sliding {
    fn new(span_ms: i64) -> Self {
        Self { span_ms, buf: VecDeque::new(), peak: 0 }
    }

    fn push(&mut self, at_ms: i64) {
        // Half-open (at - span, at]: a sighting exactly one span earlier has left.
        while self.buf.front().is_some_and(|&t| t <= at_ms - self.span_ms) {
            self.buf.pop_front();
        }
        self.buf.push_back(at_ms);
        self.peak = self.peak.max(self.buf.len() as u64);
    }

    /// Empties the window, keeping its peak.
    fn clear(&mut self) {
        self.buf.clear();
    }
}

/// An integer column as a count. Every count the store writes is unsigned.
fn count(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn bridge(conn: &Connection) -> rusqlite::Result<Option<BridgeLoss>> {
    // One row per queue a reply reported, or one with NULL queue columns for a reply with none.
    let mut stmt = conn.prepare(
        "SELECT s.id, s.rx_count, s.dropped_tx, s.uptime_ms, s.host_frames, s.rx_oversize,
                s.radio_link_packets, s.radio_link_crc_fail, s.radio_link_bad_magic,
                s.radio_link_seq_gap, s.radio_link_uart_errors, s.radio_link_radio_crc_fail,
                s.radio_link_radio_bad_magic,
                s.radio_link_radio_seq_gap, s.radio_link_radio_uart_errors, s.host_uart_errors,
                q.queue, q.capacity, q.high_water, q.dropped AS queue_dropped
         FROM bridge_status s LEFT JOIN bridge_queue q ON q.status_id = s.id
         ORDER BY s.id, q.rowid",
    )?;
    let mut rows = stmt.query([])?;
    let mut loss: Option<BridgeLoss> = None;
    // The status row the walk is on: its own counters are read once, on its first joined row.
    let mut status_id: Option<i64> = None;
    // The previous status row's figures. The first row is only a baseline.
    let mut prev: Option<(u64, u64, u64, u64, u64)> = None;
    // The previous status row's host UART errors, a P4 count that runs on through a C6 restart.
    let mut prev_host_uart: Option<u64> = None;
    // The previous status row's radio link counters, and the restarts seen by then.
    let mut prev_radio_link: Option<(RadioLinkLoss, u64)> = None;
    // Beside each queue, its previous row's drops and the restarts seen by then.
    let mut prev_queue: Vec<(u64, u64)> = Vec::new();
    while let Some(row) = rows.next()? {
        let loss = loss.get_or_insert_default();
        let id: i64 = row.get("id")?;
        if status_id != Some(id) {
            status_id = Some(id);
            let (received, dropped, uptime, host_frames, oversize) = (
                count(row.get("rx_count")?),
                count(row.get("dropped_tx")?),
                count(row.get("uptime_ms")?),
                count(row.get("host_frames")?),
                count(row.get("rx_oversize")?),
            );
            if let Some((prev_received, prev_dropped, prev_uptime, prev_host, prev_oversize)) = prev
            {
                let restarted = uptime < prev_uptime;
                loss.reboots += u64::from(restarted);
                loss.received += advance_since_boot(received, Some(prev_received), restarted);
                loss.dropped += advance_since_boot(dropped, Some(prev_dropped), restarted);
                loss.oversize += advance_since_boot(oversize, Some(prev_oversize), restarted);
                // The host's count runs from the engine's start, so it survives a bridge restart.
                loss.host_read += host_frames.saturating_sub(prev_host);
            }
            prev = Some((received, dropped, uptime, host_frames, oversize));
            let host_uart = count(row.get("host_uart_errors")?);
            loss.host_uart_errors += advance_since_boot(host_uart, prev_host_uart, false);
            prev_host_uart = Some(host_uart);

            let now = RadioLinkLoss::read(row)?;
            if let Some((before, life)) = &prev_radio_link {
                loss.radio_link.advance(&now, before, *life != loss.reboots);
            }
            prev_radio_link = Some((now, loss.reboots));
        }

        let Some(name) = row.get::<_, Option<String>>("queue")? else { continue };
        // A name this build does not know is skipped, so the rest of the report survives it.
        let Some(queue) = QueueId::from_name(&name) else { continue };
        let (capacity, high_water, dropped) = (
            count(row.get("capacity")?),
            count(row.get("high_water")?),
            count(row.get("queue_dropped")?),
        );
        let life = loss.reboots;
        let Some(index) = loss.queues.iter().position(|q| q.queue == queue) else {
            // The first row is only a baseline.
            loss.queues.push(QueueLoss { queue, capacity, peak: high_water, dropped: 0 });
            prev_queue.push((dropped, life));
            continue;
        };
        let (q, (before, before_life)) = (&mut loss.queues[index], &mut prev_queue[index]);
        q.capacity = capacity;
        q.peak = q.peak.max(high_water);
        let restarted = !queue.on_p4() && life != *before_life;
        q.dropped += advance_since_boot(dropped, Some(*before), restarted);
        (*before, *before_life) = (dropped, life);
    }
    let Some(mut loss) = loss else { return Ok(None) };
    let in_dropped_tx = loss.queues.iter().filter(|q| q.queue.in_dropped_tx());
    if let Some(dropped) = in_dropped_tx.map(|q| q.dropped).reduce(|sum, dropped| sum + dropped) {
        loss.dropped = dropped;
    }
    loss.usb_lost = loss.received.saturating_sub(loss.dropped).saturating_sub(loss.host_read);
    Ok(Some(loss))
}

/// One `host_status` row, as [`host`] reads it.
#[derive(Debug, Clone, Copy)]
struct HostRow {
    duplicate_batches: u64,
    garbled: u64,
    undecodable: u64,
    incompatible: u64,
    store_dropped: u64,
    lag_peak_us: u64,
    commit_peak_us: u64,
    queue_peak: u64,
    throttled: Option<u32>,
    soc_temp_mc: Option<i32>,
    battery_mv: Option<i32>,
}

impl HostRow {
    /// Read a row of [`host`]'s query.
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            duplicate_batches: count(row.get(0)?),
            garbled: count(row.get(1)?),
            undecodable: count(row.get(2)?),
            incompatible: count(row.get(3)?),
            store_dropped: count(row.get(4)?),
            lag_peak_us: count(row.get(5)?),
            commit_peak_us: count(row.get(6)?),
            queue_peak: count(row.get(7)?),
            throttled: row.get(8)?,
            soc_temp_mc: row.get(9)?,
            battery_mv: row.get(10)?,
        })
    }
}

fn host(conn: &Connection) -> rusqlite::Result<Option<HostLoss>> {
    let mut stmt = conn.prepare(
        "SELECT duplicate_batches, garbled, undecodable, incompatible, store_dropped, lag_peak_us,
                store_commit_peak_us, store_queue_peak, throttled, soc_temp_mc, battery_mv
         FROM host_status
         ORDER BY id",
    )?;
    let mut rows = stmt.query([])?;
    let mut loss: Option<HostLoss> = None;
    let mut first: Option<HostRow> = None;
    let mut last: Option<HostRow> = None;
    let mut last_throttled: Option<u32> = None;
    while let Some(row) = rows.next()? {
        let row = HostRow::read(row)?;
        first.get_or_insert(row);
        last = Some(row);

        let loss = loss.get_or_insert_default();
        let lag = row.lag_peak_us;
        loss.lag_over_100ms += u64::from(lag >= BEHIND_US);
        loss.lag_peak_us = loss.lag_peak_us.max(lag);
        loss.commit_peak_us = loss.commit_peak_us.max(row.commit_peak_us);
        loss.queue_peak = loss.queue_peak.max(row.queue_peak);
        if let Some(word) = row.throttled {
            *loss.under_voltage.get_or_insert(0) += u64::from(word & 0x1 != 0);
            *loss.throttled.get_or_insert(0) += u64::from(word & 0xE != 0);
            last_throttled = Some(word);
        }
        if let Some(temp) = row.soc_temp_mc {
            loss.temp_max_mc = Some(loss.temp_max_mc.map_or(temp, |max| max.max(temp)));
        }
        if let Some(mv) = row.battery_mv {
            loss.battery_min_mv = Some(loss.battery_min_mv.map_or(mv, |min| min.min(mv)));
        }
    }
    let (Some(mut loss), Some(first), Some(last)) = (loss, first, last) else { return Ok(None) };
    // The engine's counts run from its start, so what the capture saw is last less first.
    let delta = |count: fn(&HostRow) -> u64| count(&last).saturating_sub(count(&first));
    loss.duplicates = delta(|r| r.duplicate_batches);
    loss.garbled = delta(|r| r.garbled);
    loss.undecodable = delta(|r| r.undecodable);
    loss.incompatible = delta(|r| r.incompatible);
    loss.store_dropped = delta(|r| r.store_dropped);
    loss.since_boot = last_throttled.map(|word| (word >> 16) & 0xF);
    Ok(Some(loss))
}

/// One heartbeat row, as the walk needs it.
struct Beat {
    mac: Mac,
    counter: u32,
    wifi: u64,
    ble: u64,
}

fn heartbeats(
    conn: &Connection,
    board: NodeBoard,
    nodes: &mut BTreeMap<Mac, NodeLoss>,
) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT node_mac, counter, wifi_refused, ble_refused, slot, vdd_mv
         FROM heartbeat
         ORDER BY node_mac, id",
    )?;
    let mut rows = stmt.query([])?;
    let mut prev: Option<Beat> = None;
    let mut vdd_baseline: Option<u16> = None;
    while let Some(row) = rows.next()? {
        let beat = Beat {
            mac: row.get(0)?,
            counter: row.get(1)?,
            wifi: count(row.get(2)?),
            ble: count(row.get(3)?),
        };
        // Rows are grouped by node, so a new address starts a new baseline.
        if prev.as_ref().map(|prev| prev.mac) != Some(beat.mac) {
            vdd_baseline = None;
        }
        let node = nodes
            .entry(beat.mac)
            .or_insert_with(|| NodeLoss { mac: beat.mac, ..Default::default() });
        node.heartbeats += 1;
        // Rows run oldest first, so the latest heartbeat's slot stands, even one this build
        // cannot read, which is left out rather than failing the report.
        node.slot =
            row.get_ref(4)?.as_str().ok().and_then(Slot::parse).filter(|_| board.is_carrier());
        // A reading this build cannot read is no reading, likewise.
        if let Some(mv) = row.get_ref(5)?.as_i64().ok().and_then(|mv| u16::try_from(mv).ok()) {
            let baseline = *vdd_baseline.get_or_insert(mv);
            node.supply_drop |= supply_dropped_from(baseline, mv);
        }
        // A node's first heartbeat is a baseline only.
        if let Some(prev) = prev.as_ref().filter(|prev| prev.mac == beat.mac) {
            let rebooted = beat.counter.wrapping_sub(prev.counter) >= 0x8000_0000;
            node.wifi_refused += advance_since_boot(beat.wifi, Some(prev.wifi), rebooted);
            node.ble_refused += advance_since_boot(beat.ble, Some(prev.ble), rebooted);
        }
        prev = Some(beat);
    }
    Ok(())
}

fn batches(conn: &Connection, nodes: &mut BTreeMap<Mac, NodeLoss>) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("SELECT node_mac, SUM(lost) FROM batch_gap GROUP BY node_mac")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let mac: Mac = row.get(0)?;
        let lost = count(row.get(1)?);
        let node = nodes.entry(mac).or_insert_with(|| NodeLoss { mac, ..Default::default() });
        node.batches_lost += lost;
    }
    Ok(())
}
