//! The GPS receiver the view reports on: what it does for the capture's positions, what it says
//! of its own fix, and its sky while it is being read.
//!
//! The sky is the receiver's current report and never the retained fix, so a lost lock shows as
//! one while the position chain still uses the last fix.

use std::cmp::Reverse;

use chrono::{Datelike, Timelike};
use wartui_core::engine::Snapshot;
use wartui_core::gps::{GpsStatus, GpsView};
use wartui_core::nmea::{Constellation, FixMode, Sky, accuracy_from_hdop};
use wartui_core::position::PositionSource;

/// The receiver the view reports on.
#[derive(Debug, Clone, PartialEq)]
pub struct ReceiverView {
    /// What the receiver is doing for the capture's positions: the header's one phrase.
    pub summary: ReceiverSummary,
    /// What the receiver itself reports: the GPS modal's status row.
    pub status: ReceiverStatus,
    /// Where it is read from.
    pub port: ReceiverPort,
    /// Sentences that parsed.
    pub sentences: u64,
    /// Lines that failed a checksum or a field.
    pub rejected: u64,
    /// The receiver's current report, while it is being read (`status` is `Reading`).
    pub sky: Option<SkyView>,
}

/// What the receiver does for the capture's positions, first match first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiverSummary {
    /// The port failed. First: for a while after an unplug the source is still GPS, but the port
    /// is dead.
    Failed {
        /// The reader's account of it.
        reason: String,
    },
    /// It positions the rows.
    Positioning {
        /// Satellites in the solution, when the receiver said.
        satellites: Option<u8>,
    },
    /// The port has not been opened yet.
    Connecting,
    /// The search listens to a port at a rate.
    Scanning {
        /// The port.
        port: String,
        /// The rate.
        baud: u32,
    },
    /// Sentences arrive and the receiver has no fix.
    Searching,
    /// It had a fix, and the rows no longer take it.
    StaleFix,
    /// Neither the host nor the bridge has a receiver.
    NoReceiver,
}

/// What the receiver itself reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiverStatus {
    /// The port has not been opened yet.
    Connecting,
    /// The search listens to a port at a rate.
    Scanning {
        /// The port.
        port: String,
        /// The rate.
        baud: u32,
    },
    /// Every port was listened to and none was a receiver.
    NoReceiver,
    /// The port could not be opened or stopped reading.
    Failed {
        /// The reader's account of it.
        reason: String,
    },
    /// Sentences are being read. `silent_secs`: how long since the last, once past the receiver's
    /// max age.
    Reading {
        /// What its sentences say of its fix.
        fix: FixShown,
        /// Seconds since the last sentence, once past the receiver's max age.
        silent_secs: Option<i64>,
    },
}

/// What a receiver's sentences say of its fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixShown {
    /// No fix.
    Searching,
    /// No sentence has parsed.
    WaitingForSentences,
    /// A 3D fix.
    ThreeD {
        /// How it is augmented.
        augmented: Augmentation,
    },
    /// A 2D fix: no altitude.
    TwoD {
        /// How it is augmented.
        augmented: Augmentation,
    },
    /// A fix from a receiver that sends no `GSA`.
    Unspecified {
        /// How it is augmented.
        augmented: Augmentation,
    },
}

/// How a fix is augmented, from the `GGA` fix quality.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Augmentation {
    /// Plain GPS, or a quality with no name.
    None,
    /// Differential GPS.
    Dgps,
    /// RTK, fixed.
    RtkFixed,
    /// RTK, float.
    RtkFloat,
    /// Dead reckoning.
    DeadReckoning,
}

/// Where the receiver is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiverPort {
    /// The bridge's receiver, over the link.
    Bridge,
    /// A host port, at the rate the search settled on.
    Host {
        /// The port.
        port: String,
        /// The rate.
        baud: u32,
    },
    /// A host receiver whose search has not settled on a port.
    Unsettled,
}

/// The receiver's current report.
#[derive(Debug, Clone, PartialEq)]
pub struct SkyView {
    /// Satellites in the solution.
    pub used: Option<u8>,
    /// The last `RMC` date, UTC.
    pub date: Option<SkyDate>,
    /// Seconds since UTC midnight of the last `GGA` or `RMC`, fix or not.
    pub time_secs: Option<u32>,
    /// Degrees north.
    pub lat: Option<f64>,
    /// Degrees east.
    pub lon: Option<f64>,
    /// Meters above mean sea level.
    pub alt_m: Option<f64>,
    /// Ground speed in km/h.
    pub speed_kmh: Option<f64>,
    /// Horizontal dilution of precision.
    pub hdop: Option<f64>,
    /// HDOP times a nominal ranging error, in meters: an estimate, not a measurement.
    pub accuracy_m: Option<f64>,
    /// Every satellite in view: used first, then strongest first, then by system and PRN.
    pub satellites: Vec<SatelliteShown>,
}

/// A UTC date, as a receiver's `RMC` gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkyDate {
    /// The year.
    pub year: i32,
    /// The month, 1 to 12.
    pub month: u8,
    /// The day of the month, 1 to 31.
    pub day: u8,
}

/// One satellite in view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SatelliteShown {
    /// Its constellation.
    pub system: Constellation,
    /// Its PRN within that constellation.
    pub prn: u16,
    /// Signal to noise in dB-Hz. `None` when in view and not tracked.
    pub snr: Option<u8>,
    /// The solution uses it.
    pub used: bool,
}

/// `gps` as of `snapshot`.
pub(super) fn of(snapshot: &Snapshot, gps: &GpsView) -> ReceiverView {
    let reading = matches!(gps.status, GpsStatus::Searching | GpsStatus::Fixed { .. });
    ReceiverView {
        summary: summary(gps, snapshot.position.source),
        status: status(gps, snapshot.now_ms),
        port: if gps.via_bridge {
            ReceiverPort::Bridge
        } else {
            gps.settled.as_ref().map_or(ReceiverPort::Unsettled, |(port, baud)| {
                ReceiverPort::Host { port: port.clone(), baud: *baud }
            })
        },
        sentences: gps.counters.sentences,
        rejected: gps.counters.rejected,
        sky: reading.then(|| sky(&gps.sky)),
    }
}

/// A failed port first, then a GPS source, then the receiver's status.
fn summary(gps: &GpsView, source: PositionSource) -> ReceiverSummary {
    if let GpsStatus::Failed(reason) = &gps.status {
        return ReceiverSummary::Failed { reason: reason.clone() };
    }
    if source == PositionSource::Gps {
        let satellites = match gps.status {
            GpsStatus::Fixed { satellites } => satellites,
            _ => None,
        };
        return ReceiverSummary::Positioning { satellites };
    }
    match &gps.status {
        GpsStatus::Connecting => ReceiverSummary::Connecting,
        GpsStatus::Scanning { port, baud } => {
            ReceiverSummary::Scanning { port: port.clone(), baud: *baud }
        }
        GpsStatus::Searching => ReceiverSummary::Searching,
        GpsStatus::Fixed { .. } => ReceiverSummary::StaleFix,
        GpsStatus::NoReceiver => ReceiverSummary::NoReceiver,
        GpsStatus::Failed(reason) => ReceiverSummary::Failed { reason: reason.clone() },
    }
}

fn status(gps: &GpsView, now_ms: i64) -> ReceiverStatus {
    match &gps.status {
        GpsStatus::Connecting => ReceiverStatus::Connecting,
        GpsStatus::Scanning { port, baud } => {
            ReceiverStatus::Scanning { port: port.clone(), baud: *baud }
        }
        GpsStatus::NoReceiver => ReceiverStatus::NoReceiver,
        GpsStatus::Failed(reason) => ReceiverStatus::Failed { reason: reason.clone() },
        GpsStatus::Searching | GpsStatus::Fixed { .. } => {
            let max_age = i64::try_from(gps.max_age.as_millis()).unwrap_or(i64::MAX);
            let silent_secs = gps
                .heard_ms
                .filter(|heard| now_ms - heard > max_age)
                .map(|heard| (now_ms - heard) / 1000);
            ReceiverStatus::Reading { fix: fix(&gps.sky, gps.heard_ms.is_some()), silent_secs }
        }
    }
}

/// What the receiver's own sentences say about its fix, `heard` once one has parsed.
///
/// `GGA` quality 0 outranks a `GSA` mode: the two arrive separately, and a lost lock reaches `GGA`
/// while the last `GSA` may still say 3D.
fn fix(sky: &Sky, heard: bool) -> FixShown {
    let augmented = match sky.quality {
        Some(2) => Augmentation::Dgps,
        Some(4) => Augmentation::RtkFixed,
        Some(5) => Augmentation::RtkFloat,
        Some(6) => Augmentation::DeadReckoning,
        _ => Augmentation::None,
    };
    match (sky.mode, sky.quality) {
        (Some(FixMode::NoFix), _) | (_, Some(0)) => FixShown::Searching,
        (Some(FixMode::ThreeD), _) => FixShown::ThreeD { augmented },
        (Some(FixMode::TwoD), _) => FixShown::TwoD { augmented },
        (None, Some(_)) => FixShown::Unspecified { augmented },
        (None, None) if heard => FixShown::Searching,
        (None, None) => FixShown::WaitingForSentences,
    }
}

fn sky(sky: &Sky) -> SkyView {
    let mut satellites: Vec<SatelliteShown> = sky
        .satellites
        .iter()
        .map(|s| SatelliteShown { system: s.system, prn: s.prn, snr: s.snr, used: s.used })
        .collect();
    // System and PRN last, so the list holds still between snapshots.
    satellites.sort_by_key(|s| (!s.used, Reverse(s.snr), s.system, s.prn));
    SkyView {
        used: sky.used_count,
        date: sky.date.map(|date| SkyDate {
            year: date.year(),
            // A month is at most 12 and a day at most 31.
            month: u8::try_from(date.month()).unwrap_or(u8::MAX),
            day: u8::try_from(date.day()).unwrap_or(u8::MAX),
        }),
        time_secs: sky.time.map(|time| time.num_seconds_from_midnight()),
        lat: sky.lat,
        lon: sky.lon,
        alt_m: sky.alt,
        speed_kmh: sky.speed_kmh,
        hdop: sky.hdop,
        accuracy_m: sky.hdop.map(accuracy_from_hdop),
        satellites,
    }
}
