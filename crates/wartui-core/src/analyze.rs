//! What a capture lost on the way in, read back from the tables the store already writes.
//!
//! Heartbeat loss is exact. Every heartbeat carries `beat`, the node's since-boot count of
//! heartbeats sent, so a gap between consecutive beats is heartbeats lost between node and
//! host. A beat that repeats is a duplicate, a radio retransmit or a replay, and is neither a
//! received heartbeat nor a loss.
//!
//! Rows are walked in arrival order (`id`, since the store has one writer), not by `rx_at`:
//! the host's wall clock can step backwards. In arrival order a beat that falls can only be a
//! restart, and so can a `counter` (sweeps, not heartbeats) that falls. `beat` restarts at 1,
//! so the first heartbeat heard after a restart proves `beat - 1` earlier ones were lost.
//!
//! Ring refusals are reported beside the losses, not as one. A node's pending ring turns a
//! sighting away once per dwell, and the same network is usually heard again on a later
//! dwell and reported then. The count is the ring's pressure, not sightings the capture
//! lacks.
//!
//! Every since-boot count is rebased per session by the engine's `advance_since_boot`, the
//! rule it applies to the live figures, and the first row of each session is a baseline only:
//! what a bridge or node dropped before the capture began is not the capture's. The restart
//! signal differs: analyze reads it from `counter` and `beat`, and `beat` is exact. The
//! engine's epoch check stands in for `beat` live, so the two can disagree after a restart the
//! epoch check misses.

use std::collections::BTreeMap;

use rusqlite::{Connection, params};

use crate::engine::advance_since_boot;

/// What a capture lost, for the whole file or one session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LossSummary {
    /// The bridge's own counts, or `None` when the capture holds no status reply.
    pub bridge: Option<BridgeLoss>,
    /// One entry per node that sent a heartbeat or lost a batch, sorted by address.
    pub nodes: Vec<NodeLoss>,
}

/// What the bridge received and dropped during the capture. Exact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BridgeLoss {
    /// Frames the bridge received over the air.
    pub received: u64,
    /// Frames the bridge dropped because the host was not reading fast enough.
    pub dropped: u64,
    /// Times the bridge's uptime fell, which is a restart.
    pub reboots: u64,
}

/// What one node lost during the capture.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeLoss {
    /// The node's address.
    pub mac: Vec<u8>,
    /// Sighting batches lost between the node and the host. Exact, from `batch_gap`.
    pub batches_lost: u64,
    /// Distinct heartbeats the capture holds. A duplicate counts once.
    pub heartbeats: u64,
    /// Heartbeats lost between the node and the host. Exact, from gaps in `beat`.
    pub heartbeats_missed: u64,
    /// Access points the node's pending ring refused. Most are reported on a later dwell.
    pub wifi_refused: u64,
    /// Advertisers the node's pending buffer refused.
    pub ble_refused: u64,
}

/// What the capture lost, for `session` or every session.
///
/// Every figure is exact. Missed heartbeats are read from gaps in each node's `beat`
/// sequence; see the module docs for restarts and duplicates.
pub fn losses(conn: &Connection, session: Option<i64>) -> rusqlite::Result<LossSummary> {
    let bridge = bridge(conn, session)?;
    let mut nodes: BTreeMap<Vec<u8>, NodeLoss> = BTreeMap::new();
    heartbeats(conn, session, &mut nodes)?;
    batches(conn, session, &mut nodes)?;
    Ok(LossSummary { bridge, nodes: nodes.into_values().collect() })
}

/// An integer column as a count. Every count the store writes is unsigned.
fn count(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn bridge(conn: &Connection, session: Option<i64>) -> rusqlite::Result<Option<BridgeLoss>> {
    let mut stmt = conn.prepare(
        "SELECT session_id, rx_count, dropped_tx, uptime_ms FROM bridge_status
         WHERE ?1 IS NULL OR session_id = ?1
         ORDER BY session_id, id",
    )?;
    let mut rows = stmt.query(params![session])?;
    let mut loss: Option<BridgeLoss> = None;
    // The previous row's session, received, dropped and uptime.
    let mut prev: Option<(i64, u64, u64, u64)> = None;
    while let Some(row) = rows.next()? {
        let session_id: i64 = row.get(0)?;
        let (received, dropped, uptime) =
            (count(row.get(1)?), count(row.get(2)?), count(row.get(3)?));
        let loss = loss.get_or_insert_default();
        match prev {
            Some((prev_session, prev_received, prev_dropped, prev_uptime))
                if prev_session == session_id =>
            {
                let restarted = uptime < prev_uptime;
                loss.reboots += u64::from(restarted);
                loss.received += advance_since_boot(received, Some(prev_received), restarted);
                loss.dropped += advance_since_boot(dropped, Some(prev_dropped), restarted);
            }
            // The first row of a session is a baseline only.
            _ => {}
        }
        prev = Some((session_id, received, dropped, uptime));
    }
    Ok(loss)
}

/// One heartbeat row, as the walk needs it.
struct Beat {
    session_id: i64,
    mac: Vec<u8>,
    counter: u64,
    beat: u16,
    wifi: u64,
    ble: u64,
}

fn heartbeats(
    conn: &Connection,
    session: Option<i64>,
    nodes: &mut BTreeMap<Vec<u8>, NodeLoss>,
) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT session_id, node_mac, counter, beat, wifi_dropped, ble_dropped
         FROM heartbeat
         WHERE ?1 IS NULL OR session_id = ?1
         ORDER BY session_id, node_mac, id",
    )?;
    let mut rows = stmt.query(params![session])?;
    let mut prev: Option<Beat> = None;
    while let Some(row) = rows.next()? {
        let beat = Beat {
            session_id: row.get(0)?,
            mac: row.get(1)?,
            counter: count(row.get(2)?),
            beat: row.get(3)?,
            wifi: count(row.get(4)?),
            ble: count(row.get(5)?),
        };
        let node = nodes
            .entry(beat.mac.clone())
            .or_insert_with(|| NodeLoss { mac: beat.mac.clone(), ..Default::default() });
        match &prev {
            Some(prev) if prev.session_id == beat.session_id && prev.mac == beat.mac => {
                let rebooted =
                    beat.counter < prev.counter || beat.beat.wrapping_sub(prev.beat) >= 0x8000;
                node.wifi_refused += advance_since_boot(beat.wifi, Some(prev.wifi), rebooted);
                node.ble_refused += advance_since_boot(beat.ble, Some(prev.ble), rebooted);
                let (heard, missed) = beat_gap(prev.beat, beat.beat, rebooted);
                node.heartbeats += heard;
                node.heartbeats_missed += missed;
            }
            // The first heartbeat of a node in a session is a baseline only.
            _ => node.heartbeats += 1,
        }
        prev = Some(beat);
    }
    Ok(())
}

/// What one heartbeat adds after `prev`: heartbeats heard, then heartbeats missed.
///
/// After a restart, the beats before this one since boot were lost. A repeat is a duplicate
/// and adds nothing. See the module docs.
fn beat_gap(prev: u16, beat: u16, rebooted: bool) -> (u64, u64) {
    if rebooted {
        return (1, u64::from(beat.saturating_sub(1)));
    }
    match beat.wrapping_sub(prev) {
        0 => (0, 0),
        d => (1, u64::from(d - 1)),
    }
}

fn batches(
    conn: &Connection,
    session: Option<i64>,
    nodes: &mut BTreeMap<Vec<u8>, NodeLoss>,
) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT node_mac, SUM(lost) FROM batch_gap
         WHERE ?1 IS NULL OR session_id = ?1
         GROUP BY node_mac",
    )?;
    let mut rows = stmt.query(params![session])?;
    while let Some(row) = rows.next()? {
        let mac: Vec<u8> = row.get(0)?;
        let lost = count(row.get(1)?);
        let node =
            nodes.entry(mac.clone()).or_insert_with(|| NodeLoss { mac, ..Default::default() });
        node.batches_lost += lost;
    }
    Ok(())
}
