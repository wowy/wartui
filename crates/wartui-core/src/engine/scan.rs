//! The host's side of the P4's fleet planner: the commands that start and stop it, the rule
//! `wartui run` starts it by, and the plan mirrored from its reports.
//!
//! The P4 decides what every node scans (`wartui_proto::fleet`). The host asks it to start, in
//! Wi-Fi or Wi-Fi and Bluetooth, or to stop, and reads back what it is doing: the mode and pool
//! in each `Status`, and every share in each `Fleet` report. Nothing here is believed ahead of
//! the P4 saying so. Each share a report changes becomes an `assignment` row
//! ([`FleetEngine::record_share_changes`]).
//!
//! **A run starts the scan once, on its first connection** ([`Startup`]). A P4 found stopped is
//! started on Wi-Fi. A P4 found scanning, left so by an earlier run that did not quit cleanly, is
//! stopped and then started again in the mode it held, so each capture starts with every node's
//! dedup ring empty. The start waits for the P4 to report stopped, since a start that reached it
//! first would replace the stop and the rings would carry over. Each `Status` that still reports a
//! scan meanwhile sends the stop again, since a stop lost on the link would otherwise leave the run
//! waiting forever. A start sent on the first `Status` comes after that `Status` names every
//! member slot (`BridgeToHost::Status`, `members`). Only an engine told to drive a capture
//! ([`EngineConfig::auto_start`](super::EngineConfig::auto_start)) does any of this: `status`,
//! `sniff`, `pool` and `ota` look at a fleet without starting it.
//!
//! **After that, the run keeps the scan it wants** (`FleetEngine::wanted`). The P4 boots stopped
//! and keeps no mode, so a P4 that resets mid-run, often under a connection the CH343 holds up,
//! reports stopped, and the run starts it again in the mode it wants. The first `Status` of a
//! reconnect that finds the P4 scanning leaves it alone, and its mode becomes the one the run
//! wants, since another host may have changed it meanwhile. Any later `Status` naming another
//! scanning mode than the one wanted draws the start again: it may have been sent before the
//! operator's `b` reached the P4, or that start may have been lost. A start in the mode the P4
//! holds is a no-op on its core, so a stale `Status` costs one duplicate. Only a `Status` drives a
//! resend, of a start or a stop, one per `Status`: a `Fleet` report is pushed on every plan
//! change, and resending on each would multiply the traffic it answers. An operator's
//! [`Command::StopScan`], which a clean quit sends too, clears what the run wants first, so
//! nothing restarts a fleet the operator stopped.

use wartui_proto::bus::Slot;
use wartui_proto::fleet::{Report, ScanMode, Share};
use wartui_proto::link::HostToBridge;

use super::{ActionBatch, Command, FleetEngine, Now};
use crate::record::{Record, ShareChange};

/// What reported a scan mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Source {
    /// A `Status`, the answer to the engine's poll.
    Status,
    /// A `Fleet` report, pushed on every plan change.
    Fleet,
}

/// Where a run stands on starting the scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Startup {
    /// Nothing to start: this engine does not drive a capture, or it has started the scan.
    Done,
    /// The run's first `Status` decides. `bluetooth` is the operator's choice made meanwhile,
    /// which a start uses in place of Wi-Fi alone or the mode the P4 held.
    AwaitingStatus {
        /// The mode to start in, if the operator chose one.
        bluetooth: Option<bool>,
    },
    /// The P4 was found scanning and asked to stop. Once it reports stopped, the scan starts
    /// again with Bluetooth when `bluetooth`.
    Stopping {
        /// The mode to start in.
        bluetooth: bool,
    },
}

impl FleetEngine {
    /// Take an operator's instruction. Each is the bridge's to carry out or hold, so each goes out
    /// at once, most with a `GetStatus` behind it to show the change.
    pub(super) fn on_command(&mut self, command: Command, batch: &mut ActionBatch) {
        match command {
            Command::SetBluetooth(bluetooth) => match self.startup {
                // Started by the rule once the P4 answers, in the mode the operator chose.
                Startup::AwaitingStatus { .. } => {
                    self.startup = Startup::AwaitingStatus { bluetooth: Some(bluetooth) };
                }
                Startup::Stopping { .. } => self.startup = Startup::Stopping { bluetooth },
                // A fleet the operator stopped stays stopped.
                Startup::Done if self.wanted.is_some() => self.start(bluetooth, batch),
                Startup::Done => {}
            },
            Command::StopScan => {
                // First, so the stopped mode the P4 reports next restarts nothing.
                self.wanted = None;
                self.startup = Startup::Done;
                batch.urgent.push(HostToBridge::StopScan);
                batch.bulk.push(HostToBridge::GetStatus);
            }
            Command::SetExcludedSsids { ssids } => self.config.excluded_ssids = ssids,
            Command::PowerCycle { slot } => batch.bulk.push(HostToBridge::PowerCycle { slot }),
            Command::SetFanDuty { duty } => {
                batch.bulk.push(HostToBridge::SetFan { duty });
                batch.bulk.push(HostToBridge::GetStatus);
            }
            Command::SetPool { pool } => {
                batch.bulk.push(HostToBridge::SetPool { pool });
                batch.bulk.push(HostToBridge::GetStatus);
            }
        }
    }

    /// Ask the P4 to scan, and remember it as the scan this run wants.
    fn start(&mut self, bluetooth: bool, batch: &mut ActionBatch) {
        self.wanted = Some(bluetooth);
        batch.urgent.push(HostToBridge::StartScan { bluetooth });
        batch.bulk.push(HostToBridge::GetStatus);
    }

    /// The P4 reported `mode` in a `source`: take it, and move the run's start on.
    pub(super) fn on_scan_mode(&mut self, mode: ScanMode, source: Source, batch: &mut ActionBatch) {
        self.scan = Some(mode);
        let from_status = source == Source::Status;
        let first_status = from_status && !std::mem::replace(&mut self.status_seen, true);
        match (self.startup, mode) {
            (Startup::Done, ScanMode::Stopped) => {
                // The P4 reset, and boots stopped. A `Status` queued just ahead of the run's own
                // start also says stopped, and costs one duplicate start, which the P4's core
                // takes as a no-op.
                if let Some(bluetooth) = self.wanted {
                    tracing::info!(bluetooth, "bridge found stopped; restarting the scan");
                    self.start(bluetooth, batch);
                }
            }
            (Startup::Done, ScanMode::Wifi | ScanMode::WifiBluetooth) => {
                let bluetooth = mode == ScanMode::WifiBluetooth;
                match self.wanted {
                    // A reconnect adopts what the P4 is doing.
                    Some(_) if first_status => self.wanted = Some(bluetooth),
                    Some(wanted) if from_status && wanted != bluetooth => {
                        tracing::info!(?mode, wanted, "bridge scanning another mode; asking again");
                        batch.urgent.push(HostToBridge::StartScan { bluetooth: wanted });
                    }
                    _ => {}
                }
            }
            (Startup::AwaitingStatus { bluetooth }, ScanMode::Stopped) => {
                self.startup = Startup::Done;
                self.start(bluetooth.unwrap_or(false), batch);
            }
            (Startup::AwaitingStatus { bluetooth }, ScanMode::Wifi | ScanMode::WifiBluetooth) => {
                let bluetooth = bluetooth.unwrap_or(mode == ScanMode::WifiBluetooth);
                tracing::info!(?mode, "bridge found scanning; stopping it to start this capture");
                self.startup = Startup::Stopping { bluetooth };
                stop(batch);
            }
            (Startup::Stopping { bluetooth }, ScanMode::Stopped) => {
                self.startup = Startup::Done;
                self.start(bluetooth, batch);
            }
            // The stop may have been lost on the link. The next poll's `Status` says whether it
            // landed, so no `GetStatus` goes with it.
            (Startup::Stopping { .. }, ScanMode::Wifi | ScanMode::WifiBluetooth) if from_status => {
                batch.urgent.push(HostToBridge::StopScan);
            }
            (Startup::Stopping { .. }, ScanMode::Wifi | ScanMode::WifiBluetooth) => {}
        }
    }

    /// A new connection. The mirror is the last P4's, so it goes. A stop sent before the link
    /// dropped may have been lost, so it goes again.
    pub(super) fn on_connected_scan(&mut self, batch: &mut ActionBatch) {
        self.fleet = None;
        self.status_seen = false;
        if let Startup::Stopping { .. } = self.startup {
            stop(batch);
        }
    }

    /// The P4's plan, from a `Fleet` report. Its mode completes a stop, but the run's first
    /// decision waits for a `Status`, the answer to the engine's own poll.
    pub(super) fn on_fleet(&mut self, report: &Report, now: Now, batch: &mut ActionBatch) {
        self.record_share_changes(report, now, batch);
        self.fleet = Some(report.clone());
        if let Startup::AwaitingStatus { .. } = self.startup {
            self.scan = Some(report.mode);
        } else {
            self.on_scan_mode(report.mode, Source::Fleet, batch);
        }
    }

    /// Record each slot whose share `report` changes: a share listed that differs from the one
    /// last recorded for its slot, and a slot last recorded with a share that `report` leaves out,
    /// as withdrawn. The comparison is against the last report of any connection, so the first
    /// report after a reconnect records only what differs from the last one before it.
    fn record_share_changes(&mut self, report: &Report, now: Now, batch: &mut ActionBatch) {
        for share in &report.shares {
            if self.recorded_shares.get(&share.slot) != Some(share) {
                self.recorded_shares.insert(share.slot, *share);
                batch.records.push(Record::Share(ShareChange {
                    rx_at_ms: now.unix_ms,
                    slot: share.slot,
                    epoch: share.epoch,
                    channels: share.channels,
                    ble: share.ble,
                }));
            }
        }
        let withdrawn: Vec<Slot> = self
            .recorded_shares
            .keys()
            .filter(|slot| !report.shares.iter().any(|share| share.slot == **slot))
            .copied()
            .collect();
        for slot in withdrawn {
            self.recorded_shares.remove(&slot);
            batch.records.push(Record::Share(ShareChange::withdrawn(now.unix_ms, slot)));
        }
    }

    /// The share the P4's last report gives the node in `slot`, if any.
    pub(super) fn share_at(&self, slot: Slot) -> Option<Share> {
        self.fleet.as_ref()?.shares.iter().find(|share| share.slot == slot).copied()
    }
}

/// Ask the P4 to stop, then for the `Status` that shows it.
fn stop(batch: &mut ActionBatch) {
    batch.urgent.push(HostToBridge::StopScan);
    batch.bulk.push(HostToBridge::GetStatus);
}
