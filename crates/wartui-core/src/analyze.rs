//! What a capture lost on the way in, read back from the tables the store already writes.
//!
//! Heartbeat loss comes from sequences. Every heartbeat carries `beat`, the node's since-boot count of
//! heartbeats sent, so a gap between consecutive beats is heartbeats lost. A repeated beat, a radio
//! retransmit or a replay, is neither received nor lost.
//!
//! Rows are walked in arrival order (`id`, since the store has one writer), not by `rx_at`, which
//! the wall clock can step back. Forward modulo deltas below half-range in `beat` (u16) and
//! `counter` (u32 sweeps, not heartbeats) include wraps; a delta at least half-range indicates a
//! restart. This assumes fewer than 32,768 beats and 2^31 sweeps between observations. A restart
//! whose counters both look forward cannot be detected. `beat` restarts at 1, so the first
//! heartbeat after a detected restart contributes `beat - 1` missed beats since boot.
//!
//! Each counted gap keeps the two observed arrival timestamps and row IDs. These bracket missing
//! beats, not their exact transmission times. A restart-associated interval is uncertain: the
//! boot time and any loss before that boot are unknown. Clock reversals remain visible, never
//! sorted away. Windows and lifetime totals use the same `beat_gap` result in the same walk.
//!
//! A gap counts only after a heartbeat that arrived live. One replayed from the bridge's backlog
//! can be minutes older than the next, and the heartbeats between were sent while no host read. The
//! bridge's `dropped` covers those, before the capture's baseline. The few lost at the
//! replay-to-live handoff go uncounted.
//!
//! Ring refusals are reported beside the losses, not as one. A pending ring turns a sighting away
//! once per dwell, and the network is usually reported on a later dwell. The count is ring
//! pressure, not missing sightings.
//!
//! On the bridge side, each `bridge_status` row carries `host_frames`, the frames the host had read
//! when the reply arrived. `host_read` is the frames read between the first reply and the last.
//! `received` less the bridge's drops less `host_read` is the frames lost between the bridge's
//! queue and the host. It is approximate: a status reply is a priority frame and overtakes the
//! frames queued in the bulk ring, which `rx_count` counts and `host_frames` does not yet. So it is
//! off by up to the ring's 24 frames at each end, and saturates at 0 when a baseline taken
//! mid-backlog shows the host reading more than the bridge passed on.
//!
//! On the host side, [`HostLoss`] breaks down what the host read and did not store, from the
//! `host_status` rows. Their counts run from the engine's start and are read as the last row less
//! the first, which is written as the capture starts. Peaks are per row, so it takes the largest,
//! and counts the rows whose lag peak reached the 100 ms that makes a heartbeat too stale to
//! answer.
//!
//! Every since-boot count is rebased by the engine's `advance_since_boot`, and the first row is a
//! baseline only (per node, for heartbeats): what was dropped before the capture is not the
//! capture's. Analyze infers restarts from `counter` and `beat`. Live, the engine's
//! epoch check stands in for `beat`, so the two can disagree after a restart the epoch check
//! misses.

use std::collections::BTreeMap;

use rusqlite::{Connection, Row};
use wartui_proto::mac::Mac;

use crate::engine::{BEHIND_THE_AIR_US, advance_since_boot};

/// What a capture lost.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LossSummary {
    /// The bridge's own counts, or `None` when the capture holds no status reply.
    pub bridge: Option<BridgeLoss>,
    /// One entry per node that sent a heartbeat or lost a batch, sorted by address.
    pub nodes: Vec<NodeLoss>,
    /// The host's own figures, or `None` when the capture holds no `host_status` row.
    pub host: Option<HostLoss>,
}

/// What the bridge received and dropped during the capture. Exact but for `usb_lost`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BridgeLoss {
    /// Frames the bridge received over the air.
    pub received: u64,
    /// Frames the bridge dropped because the host was not reading fast enough.
    pub dropped: u64,
    /// Times the bridge's uptime fell, which is a restart.
    pub reboots: u64,
    /// Frames the host read off the link between the first status reply and the last.
    pub host_read: u64,
    /// Frames lost between the bridge's queue and the host: `received` less `dropped` less
    /// `host_read`, saturating at 0. Approximate to the 24 frames the bulk ring can queue at each
    /// end, which a status reply overtakes.
    pub usb_lost: u64,
}

/// What the host did with the frames it read, and how it held up, first `host_status` row to last.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostLoss {
    /// Sighting batches dropped as radio retransmits.
    pub duplicates: u64,
    /// USB frames that failed their checksum.
    pub garbled: u64,
    /// Frames that were nobody's recognisable format.
    pub undecodable: u64,
    /// Frames from another fleet, another core, or a build speaking another wire version.
    pub foreign: u64,
    /// Assignments held back because the heartbeat that would have carried them was stale.
    pub admin_windows_missed: u64,
    /// Rows the store dropped.
    pub store_dropped: u64,
    /// Rows whose lag peak reached 100 ms: samples in which the host fell behind the air.
    pub lag_over_100ms: u64,
    /// The furthest behind the air the host fell, in microseconds.
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
    /// Sighting batches lost between the node and the host. Exact, from `batch_gap`.
    pub batches_lost: u64,
    /// Distinct heartbeats the capture holds. A duplicate counts once.
    pub heartbeats: u64,
    /// Heartbeats lost between the node and the host: gaps in `beat` after a live heartbeat.
    pub heartbeats_missed: u64,
    /// Counted gaps in arrival order. Their missed counts sum to `heartbeats_missed`.
    pub heartbeat_windows: Vec<HeartbeatWindow>,
    /// Access points the node's pending ring refused. Most are reported on a later dwell.
    pub wifi_refused: u64,
    /// Advertisers the node's pending buffer refused.
    pub ble_refused: u64,
}

/// Observed arrivals bracketing a counted heartbeat gap, not exact transmission times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeartbeatWindow {
    /// Previous heartbeat row ID, which preserves arrival order even if the clock reverses.
    pub start_id: i64,
    /// Current heartbeat row ID.
    pub end_id: i64,
    /// Previous heartbeat's UTC arrival time, in unix milliseconds.
    pub start_rx_at_ms: i64,
    /// Current heartbeat's UTC arrival time; it can be earlier than `start_rx_at_ms`.
    pub end_rx_at_ms: i64,
    /// Missing beats, from the same sequence accounting as the lifetime total.
    pub missed: u64,
    /// A detected restart makes the interval uncertain: only missing beats since boot count.
    pub restarted: bool,
}

/// What the capture lost. Missed heartbeats follow the module's modulo assumptions;
/// the bridge's `usb_lost` is approximate.
pub fn losses(conn: &Connection) -> rusqlite::Result<LossSummary> {
    let bridge = bridge(conn)?;
    let mut nodes: BTreeMap<Mac, NodeLoss> = BTreeMap::new();
    heartbeats(conn, &mut nodes)?;
    batches(conn, &mut nodes)?;
    Ok(LossSummary { bridge, nodes: nodes.into_values().collect(), host: host(conn)? })
}

/// An integer column as a count. Every count the store writes is unsigned.
fn count(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn bridge(conn: &Connection) -> rusqlite::Result<Option<BridgeLoss>> {
    let mut stmt = conn.prepare(
        "SELECT rx_count, dropped_tx, uptime_ms, host_frames FROM bridge_status ORDER BY id",
    )?;
    let mut rows = stmt.query([])?;
    let mut loss: Option<BridgeLoss> = None;
    // The previous row's figures. The first row is only a baseline.
    let mut prev: Option<(u64, u64, u64, u64)> = None;
    while let Some(row) = rows.next()? {
        let (received, dropped, uptime, host_frames) =
            (count(row.get(0)?), count(row.get(1)?), count(row.get(2)?), count(row.get(3)?));
        let loss = loss.get_or_insert_default();
        if let Some((prev_received, prev_dropped, prev_uptime, prev_host)) = prev {
            let restarted = uptime < prev_uptime;
            loss.reboots += u64::from(restarted);
            loss.received += advance_since_boot(received, Some(prev_received), restarted);
            loss.dropped += advance_since_boot(dropped, Some(prev_dropped), restarted);
            // The host's count runs from the engine's start, so it survives a bridge restart.
            loss.host_read += host_frames.saturating_sub(prev_host);
        }
        prev = Some((received, dropped, uptime, host_frames));
    }
    if let Some(loss) = &mut loss {
        loss.usb_lost = loss.received.saturating_sub(loss.dropped).saturating_sub(loss.host_read);
    }
    Ok(loss)
}

/// One `host_status` row, as [`host`] reads it.
#[derive(Debug, Clone, Copy)]
struct HostRow {
    duplicate_batches: u64,
    garbled: u64,
    undecodable: u64,
    incompatible: u64,
    foreign_fleet: u64,
    foreign_admin: u64,
    admin_windows_missed: u64,
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
            foreign_fleet: count(row.get(4)?),
            foreign_admin: count(row.get(5)?),
            admin_windows_missed: count(row.get(6)?),
            store_dropped: count(row.get(7)?),
            lag_peak_us: count(row.get(8)?),
            commit_peak_us: count(row.get(9)?),
            queue_peak: count(row.get(10)?),
            throttled: row.get(11)?,
            soc_temp_mc: row.get(12)?,
            battery_mv: row.get(13)?,
        })
    }
}

fn host(conn: &Connection) -> rusqlite::Result<Option<HostLoss>> {
    let mut stmt = conn.prepare(
        "SELECT duplicate_batches, garbled, undecodable, incompatible, foreign_fleet,
                foreign_admin, admin_windows_missed, store_dropped,
                lag_peak_us, store_commit_peak_us, store_queue_peak, throttled, soc_temp_mc,
                battery_mv
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
        loss.lag_over_100ms += u64::from(lag >= BEHIND_THE_AIR_US);
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
    loss.foreign =
        delta(|r| r.incompatible) + delta(|r| r.foreign_fleet) + delta(|r| r.foreign_admin);
    loss.admin_windows_missed = delta(|r| r.admin_windows_missed);
    loss.store_dropped = delta(|r| r.store_dropped);
    loss.since_boot = last_throttled.map(|word| (word >> 16) & 0xF);
    Ok(Some(loss))
}

/// One heartbeat row, as the walk needs it.
struct Beat {
    mac: Mac,
    counter: u32,
    beat: u16,
    wifi: u64,
    ble: u64,
    live: bool,
    id: i64,
    rx_at_ms: i64,
}

fn heartbeats(conn: &Connection, nodes: &mut BTreeMap<Mac, NodeLoss>) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT node_mac, counter, beat, wifi_dropped, ble_dropped, live, id, rx_at
         FROM heartbeat
         ORDER BY node_mac, id",
    )?;
    let mut rows = stmt.query([])?;
    let mut prev: Option<Beat> = None;
    while let Some(row) = rows.next()? {
        let beat = Beat {
            mac: row.get(0)?,
            counter: row.get(1)?,
            beat: row.get(2)?,
            wifi: count(row.get(3)?),
            ble: count(row.get(4)?),
            live: row.get(5)?,
            id: row.get(6)?,
            rx_at_ms: row.get(7)?,
        };
        let node = nodes
            .entry(beat.mac)
            .or_insert_with(|| NodeLoss { mac: beat.mac, ..Default::default() });
        match &prev {
            Some(prev) if prev.mac == beat.mac => {
                let rebooted = beat.counter.wrapping_sub(prev.counter) >= 0x8000_0000
                    || beat.beat.wrapping_sub(prev.beat) >= 0x8000;
                node.wifi_refused += advance_since_boot(beat.wifi, Some(prev.wifi), rebooted);
                node.ble_refused += advance_since_boot(beat.ble, Some(prev.ble), rebooted);
                let (heard, missed) = beat_gap(prev.beat, beat.beat, rebooted);
                node.heartbeats += heard;
                // A gap after a replayed heartbeat spans time no host was reading.
                if prev.live && missed > 0 {
                    node.heartbeats_missed += missed;
                    node.heartbeat_windows.push(HeartbeatWindow {
                        start_id: prev.id,
                        end_id: beat.id,
                        start_rx_at_ms: prev.rx_at_ms,
                        end_rx_at_ms: beat.rx_at_ms,
                        missed,
                        restarted: rebooted,
                    });
                }
            }
            // A node's first heartbeat is a baseline only.
            _ => node.heartbeats += 1,
        }
        prev = Some(beat);
    }
    Ok(())
}

/// What one heartbeat adds after `prev`: heartbeats heard, then missed. After a restart, every
/// earlier beat since boot was lost. A repeat adds nothing.
fn beat_gap(prev: u16, beat: u16, rebooted: bool) -> (u64, u64) {
    if rebooted {
        return (1, u64::from(beat.saturating_sub(1)));
    }
    match beat.wrapping_sub(prev) {
        0 => (0, 0),
        d => (1, u64::from(d - 1)),
    }
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
