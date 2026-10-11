//! Reading a position out of an NMEA 0183 stream.
//!
//! A small parser: two sentence types, which carry everything the store has a column for. `GGA` has
//! altitude, satellite count and dilution of precision. `RMC` has the date, without which no
//! timestamp can be built. Everything else (`GSV`, `GSA`, `VTG`, proprietary `PMTK` chatter) parses
//! as valid and unused.
//!
//! **`GSA` and `GSV` feed diagnostics only.** They build the [`Sky`] the view's GPS modal shows:
//! which satellites are in view, how strong, and which the solution uses. Nothing in the store or
//! the position chain reads it, so they still report [`Report::Other`], and a malformed one is
//! skipped rather than rejected.
//!
//! A `GSV` group is keyed by constellation and signal ID, so a dual-band receiver's L1 and L5 lists
//! do not overwrite each other, and it replaces the last one only once its final message arrives.
//! A run of consecutive `GSA` sentences is one epoch's used set. Each names its constellation with
//! the NMEA 4.10 system ID, or with its talker, or not at all: an NMEA 4.0 receiver sends `GNGSA`
//! with neither, and matching those on the PRN alone is correct because its PRN ranges are
//! disjoint.
//!
//! **The checksum is mandatory.** Not for line noise on a USB CDC endpoint that has CRCs already,
//! but because the first read after opening lands mid-sentence, and half of
//! `$GPGGA,123519,4807.038,N,…` is a well-formed sentence about somewhere else.

use std::collections::{BTreeMap, BTreeSet};
use std::str::from_utf8;

use chrono::{NaiveDate, NaiveTime};

/// Why a line was not a usable sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NmeaError {
    /// No leading `$`, or too short to hold a talker and a type.
    #[error("not an NMEA sentence")]
    NotASentence,
    /// Missing, malformed, or mismatched `*HH` checksum.
    #[error("checksum mismatch")]
    Checksum,
    /// The right shape, but a field that should have been a number was not.
    #[error("malformed field")]
    Malformed,
}

/// What one sentence told us.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Report {
    /// The receiver reported a position.
    Fix(GpsFix),
    /// A positional sentence saying the receiver has no fix yet. Distinct from
    /// [`Report::Other`]: "searching" rather than "talking about satellites".
    NoFix,
    /// A valid sentence this parser has no use for.
    Other,
}

/// A position as a receiver reported it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GpsFix {
    /// Degrees north, negative for south.
    pub lat: f64,
    /// Degrees east, negative for west.
    pub lon: f64,
    /// Meters above mean sea level, when the sentence carried it.
    pub alt: Option<f64>,
    /// Estimated horizontal accuracy in meters. See [`accuracy_from_hdop`].
    pub accuracy: Option<f64>,
    /// Satellites used in the solution, when reported.
    pub satellites: Option<u8>,
    /// UTC of the fix in Unix milliseconds, when the date is known.
    ///
    /// `GGA` has a time but no date, so this stays `None` until an `RMC` arrives. A fix is usable
    /// without it, and it keeps the receiver's clock visible rather than silently replaced by the
    /// host's.
    pub at_ms: Option<i64>,
}

/// HDOP is a multiplier, not a distance: how much satellite geometry amplifies ranging error. Times
/// a nominal 5 m user-equivalent range error gives meters, and WiGLE's `AccuracyMeters` wants a
/// number: an honest estimate beats 0.
#[must_use]
pub fn accuracy_from_hdop(hdop: f64) -> f64 {
    hdop * 5.0
}

/// What the receiver says about its fix and its sky, for diagnostics.
///
/// Position fields are the receiver's current report and clear when it reports no fix. They never
/// fall back to a retained fix; that is the position chain's concern.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sky {
    /// `GGA` fix quality: 0 none, 1 GPS, 2 DGPS, 4 RTK fixed, 5 RTK float, 6 dead reckoning.
    pub quality: Option<u8>,
    /// `GSA` fix mode.
    pub mode: Option<FixMode>,
    /// Satellites in the solution, from `GGA`.
    pub used_count: Option<u8>,
    /// UTC time of the last `GGA` or `RMC`, fix or not.
    pub time: Option<NaiveTime>,
    /// The last `RMC` date.
    pub date: Option<NaiveDate>,
    /// Degrees north, from the current `GGA`.
    pub lat: Option<f64>,
    /// Degrees east, from the current `GGA`.
    pub lon: Option<f64>,
    /// Meters above mean sea level, from the current `GGA`.
    pub alt: Option<f64>,
    /// Horizontal dilution of precision, from `GGA`.
    pub hdop: Option<f64>,
    /// Ground speed from `RMC`, in km/h.
    pub speed_kmh: Option<f64>,
    /// Every satellite in view, one entry per constellation and PRN, ordered by both.
    pub satellites: Vec<Satellite>,
}

impl Sky {
    const fn new() -> Self {
        Self {
            quality: None,
            mode: None,
            used_count: None,
            time: None,
            date: None,
            lat: None,
            lon: None,
            alt: None,
            hdop: None,
            speed_kmh: None,
            satellites: Vec::new(),
        }
    }
}

/// One satellite in view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Satellite {
    /// Its constellation. PRNs repeat across constellations.
    pub system: Constellation,
    /// Its PRN within that constellation.
    pub prn: u16,
    /// Signal to noise in dB-Hz. `None` when in view but not tracked.
    pub snr: Option<u8>,
    /// Whether the solution uses it.
    pub used: bool,
}

/// A satellite navigation system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Constellation {
    /// The US system.
    Gps,
    /// The Russian system.
    Glonass,
    /// The European system.
    Galileo,
    /// The Chinese system.
    BeiDou,
    /// The Japanese regional system.
    Qzss,
    /// The Indian regional system.
    NavIc,
}

impl Constellation {
    /// A sentence's talker, or `None` for `GN` (combined) and anything unknown.
    fn from_talker(talker: &str) -> Option<Self> {
        Some(match talker {
            "GP" => Self::Gps,
            "GL" => Self::Glonass,
            "GA" => Self::Galileo,
            "GB" | "BD" => Self::BeiDou,
            "GQ" => Self::Qzss,
            "GI" => Self::NavIc,
            _ => return None,
        })
    }

    /// An NMEA 4.10 system ID.
    fn from_system_id(id: &str) -> Option<Self> {
        Some(match id {
            "1" => Self::Gps,
            "2" => Self::Glonass,
            "3" => Self::Galileo,
            "4" => Self::BeiDou,
            "5" => Self::Qzss,
            "6" => Self::NavIc,
            _ => return None,
        })
    }
}

/// The `GSA` fix mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixMode {
    /// No fix.
    NoFix,
    /// A 2D fix: no altitude.
    TwoD,
    /// A 3D fix.
    ThreeD,
}

/// One constellation and signal's `GSV` lists: the group being assembled and the last whole one.
#[derive(Debug, Clone, Default)]
struct GsvGroup {
    /// The group being assembled and the message number it expects next.
    pending: Option<(Vec<Satellite>, u8)>,
    /// The last group whose final message arrived.
    committed: Vec<Satellite>,
}

/// Milliseconds in a day, and in the half day that separates "the clock has
/// wrapped past midnight" from "this receiver is confused".
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const HALF_DAY_MS: i64 = DAY_MS / 2;

/// A running parse of one receiver's output.
/// Holds the date, which arrives in `RMC` and stamps the `GGA` sentences between, and the [`Sky`].
#[derive(Debug, Clone, Default)]
pub struct Nmea {
    date: Option<NaiveDate>,
    last_stamp_ms: Option<i64>,
    sky: Sky,
    gsv: BTreeMap<(Constellation, Option<u8>), GsvGroup>,
    /// The current epoch's used satellites. `None` system matches any constellation.
    used: BTreeSet<(Option<Constellation>, u16)>,
    /// Whether the last sentence was a `GSA`, so the next one continues the run.
    in_gsa_run: bool,
}

impl Nmea {
    /// A parser that has not yet seen a date.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            date: None,
            last_stamp_ms: None,
            sky: Sky::new(),
            gsv: BTreeMap::new(),
            used: BTreeSet::new(),
            in_gsa_run: false,
        }
    }

    /// What the receiver has said about its fix and its sky.
    #[must_use]
    pub const fn sky(&self) -> &Sky {
        &self.sky
    }

    /// Parse one line.
    ///
    /// # Errors
    /// [`NmeaError`] if the line is not a sentence, fails its checksum, or has
    /// a field that cannot be read as the number it should be.
    pub fn parse(&mut self, line: &[u8]) -> Result<Report, NmeaError> {
        let body = checked_body(line)?;
        let mut fields = body.split(',');
        let id = fields.next().ok_or(NmeaError::NotASentence)?;
        // Talker IDs vary by constellation and a receiver changes its own as
        // satellites come and go, so only the last three characters decide.
        let kind = id.get(id.len().saturating_sub(3)..).ok_or(NmeaError::NotASentence)?;
        let talker = id.get(..id.len().saturating_sub(3)).unwrap_or_default();
        let fields: Vec<&str> = fields.collect();
        if kind != "GSA" {
            self.in_gsa_run = false;
        }
        match kind {
            "GGA" => {
                let report = self.gga(&fields)?;
                self.sky_gga(&fields);
                Ok(report)
            }
            "RMC" => {
                let report = self.rmc(&fields)?;
                self.sky_rmc(&fields);
                Ok(report)
            }
            "GSA" => {
                self.gsa(talker, &fields);
                Ok(Report::Other)
            }
            "GSV" => {
                self.gsv(talker, &fields);
                Ok(Report::Other)
            }
            _ => Ok(Report::Other),
        }
    }

    /// The [`Sky`] half of a `GGA` that parsed.
    fn sky_gga(&mut self, f: &[&str]) {
        let sky = &mut self.sky;
        sky.time = time(field(f, 0));
        sky.quality = integer(field(f, 5)).ok().flatten();
        sky.used_count = integer(field(f, 6)).ok().flatten();
        let fixed = sky.quality.is_some_and(|q| q != 0);
        let lenient = |c: Result<Option<f64>, NmeaError>| c.ok().flatten().filter(|_| fixed);
        // A receiver with no fix reports a placeholder HDOP (u-blox sends 99.99).
        sky.hdop = lenient(number(field(f, 7)));
        sky.lat = lenient(coord(f, 1));
        sky.lon = lenient(coord(f, 3));
        sky.alt = lenient(number(field(f, 8)));
    }

    /// The [`Sky`] half of an `RMC` that parsed.
    fn sky_rmc(&mut self, f: &[&str]) {
        self.sky.time = time(field(f, 0));
        self.sky.date = self.date;
        self.sky.speed_kmh = if field(f, 1) == "A" {
            number(field(f, 6)).ok().flatten().map(|knots| knots * 1.852)
        } else {
            None
        };
    }

    /// `$--GSA,mode,fix,prn×12,pdop,hdop,vdop[,system]`
    fn gsa(&mut self, talker: &str, f: &[&str]) {
        if !self.in_gsa_run {
            self.used.clear();
            self.in_gsa_run = true;
        }
        self.sky.mode = match field(f, 1) {
            "1" => Some(FixMode::NoFix),
            "2" => Some(FixMode::TwoD),
            "3" => Some(FixMode::ThreeD),
            _ => self.sky.mode,
        };
        let system = Constellation::from_system_id(field(f, 17))
            .or_else(|| Constellation::from_talker(talker));
        for prn in (2..14).filter_map(|i| field(f, i).parse::<u16>().ok()) {
            self.used.insert((system, prn));
        }
        self.merge();
    }

    /// `$--GSV,total,number,in_view,{prn,elevation,azimuth,snr}×n[,signal]`
    fn gsv(&mut self, talker: &str, f: &[&str]) {
        let Some(system) = Constellation::from_talker(talker) else { return };
        let (Ok(total), Ok(number)) = (field(f, 0).parse::<u8>(), field(f, 1).parse::<u8>()) else {
            return;
        };
        // NMEA 4.10 appends a signal ID, which leaves one field over the satellite quads.
        let signal_id = f.len() >= 3 && (f.len() - 3) % 4 == 1;
        let signal = if signal_id { f.last().and_then(|s| s.parse().ok()) } else { None };
        let quads = f.get(3..f.len() - usize::from(signal_id)).unwrap_or_default();
        let satellites = quads.as_chunks::<4>().0.iter().filter_map(|[prn, _, _, snr]| {
            let prn = prn.parse().ok()?;
            Some(Satellite { system, prn, snr: snr.parse().ok(), used: false })
        });

        let group = self.gsv.entry((system, signal)).or_default();
        let mut pending = match group.pending.take() {
            _ if number == 1 => Vec::new(),
            Some((list, next)) if next == number => list,
            // Out of sequence: a lost message would leave the group short.
            _ => return,
        };
        pending.extend(satellites);
        if number >= total {
            group.committed = pending;
            self.merge();
        } else {
            group.pending = Some((pending, number + 1));
        }
    }

    /// Rebuild [`Sky::satellites`] from the committed `GSV` groups and the `GSA` used set.
    fn merge(&mut self) {
        let mut merged: BTreeMap<(Constellation, u16), Satellite> = BTreeMap::new();
        for satellite in self.gsv.values().flat_map(|group| &group.committed) {
            let entry = merged.entry((satellite.system, satellite.prn)).or_insert(*satellite);
            entry.snr = entry.snr.max(satellite.snr);
        }
        self.sky.satellites = merged
            .into_values()
            .map(|mut s| {
                s.used = self.used.contains(&(Some(s.system), s.prn))
                    || self.used.contains(&(None, s.prn));
                s
            })
            .collect();
    }

    /// `$--GGA,time,lat,N,lon,E,quality,sats,hdop,alt,M,…`
    fn gga(&mut self, f: &[&str]) -> Result<Report, NmeaError> {
        let time = field(f, 0);
        // Quality 0 is no fix. Anything else is a position the receiver stands behind.
        if integer(field(f, 5))?.unwrap_or(0) == 0 {
            return Ok(Report::NoFix);
        }
        let (Some(lat), Some(lon)) = (coord(f, 1)?, coord(f, 3)?) else {
            return Ok(Report::NoFix);
        };
        Ok(Report::Fix(GpsFix {
            lat,
            lon,
            alt: number(field(f, 8))?,
            accuracy: number(field(f, 7))?.map(accuracy_from_hdop),
            satellites: integer(field(f, 6))?,
            at_ms: self.stamp(time),
        }))
    }

    /// `$--RMC,time,status,lat,N,lon,E,speed,track,date,…`
    fn rmc(&mut self, f: &[&str]) -> Result<Report, NmeaError> {
        let time = field(f, 0);
        // Worth keeping even from a sentence with no fix: time but no position is
        // the normal state for the first half-minute after a cold start.
        if let Some(date) = date(field(f, 8))? {
            self.date = Some(date);
        }
        if field(f, 1) != "A" {
            return Ok(Report::NoFix);
        }
        let (Some(lat), Some(lon)) = (coord(f, 2)?, coord(f, 4)?) else {
            return Ok(Report::NoFix);
        };
        Ok(Report::Fix(GpsFix {
            lat,
            lon,
            alt: None,
            accuracy: None,
            satellites: None,
            at_ms: self.stamp(time),
        }))
    }

    /// Combine the date carried from the last `RMC` with a sentence's time.
    fn stamp(&mut self, time: &str) -> Option<i64> {
        let date = self.date?;
        let hour: u32 = time.get(0..2)?.parse().ok()?;
        let minute: u32 = time.get(2..4)?.parse().ok()?;
        let second: f64 = time.get(4..)?.parse().ok()?;
        let milli = u32::try_from((second * 1000.0).round() as i64).ok()?;
        // A leap second arrives as :60, which chrono represents as :59 with
        // more than a thousand milliseconds rather than as a sixtieth second.
        let (secs, millis) =
            if milli >= 60_000 { (59, milli - 59_000) } else { (milli / 1000, milli % 1000) };
        let at_ms =
            date.and_hms_milli_opt(hour, minute, secs, millis)?.and_utc().timestamp_millis();
        // A `GGA` borrows the previous `RMC`'s date, so sentences between midnight and that cycle's
        // `RMC` land a day early. Only that runs time backwards by hours.
        let at_ms = match self.last_stamp_ms {
            Some(last) if at_ms < last - HALF_DAY_MS => at_ms + DAY_MS,
            _ => at_ms,
        };
        self.last_stamp_ms = Some(at_ms);
        Some(at_ms)
    }
}

/// `hhmmss.ss`, or `None` when empty or unreadable.
fn time(text: &str) -> Option<NaiveTime> {
    let hour = text.get(0..2)?.parse().ok()?;
    let minute = text.get(2..4)?.parse().ok()?;
    let second: f64 = text.get(4..)?.parse().ok()?;
    NaiveTime::from_hms_milli_opt(hour, minute, second as u32, (second.fract() * 1000.0) as u32)
}

/// Verify the `*HH` checksum and hand back the part it covers.
fn checked_body(line: &[u8]) -> Result<&str, NmeaError> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let rest = line.strip_prefix(b"$").ok_or(NmeaError::NotASentence)?;
    let star = rest.iter().rposition(|b| *b == b'*').ok_or(NmeaError::Checksum)?;
    let (body, tail) = rest.split_at(star);
    let digits = tail.get(1..3).ok_or(NmeaError::Checksum)?;
    if tail.len() != 3 {
        return Err(NmeaError::Checksum);
    }
    let want = u8::from_str_radix(from_utf8(digits).map_err(|_| NmeaError::Checksum)?, 16)
        .map_err(|_| NmeaError::Checksum)?;
    if body.iter().fold(0u8, |acc, b| acc ^ b) != want {
        return Err(NmeaError::Checksum);
    }
    let body = from_utf8(body).map_err(|_| NmeaError::Malformed)?;
    if body.len() < 5 {
        return Err(NmeaError::NotASentence);
    }
    Ok(body)
}

/// A field by index, or empty. NMEA leaves unknown fields empty, and a short sentence says the same
/// by stopping early.
fn field<'a>(fields: &[&'a str], index: usize) -> &'a str {
    fields.get(index).copied().unwrap_or_default()
}

/// An empty field is a receiver saying it does not know, which is not an error.
fn number(text: &str) -> Result<Option<f64>, NmeaError> {
    if text.is_empty() {
        return Ok(None);
    }
    text.parse().map(Some).map_err(|_| NmeaError::Malformed)
}

/// A whole small number, in a field where a fraction would be nonsense.
fn integer(text: &str) -> Result<Option<u8>, NmeaError> {
    if text.is_empty() {
        return Ok(None);
    }
    text.parse().map(Some).map_err(|_| NmeaError::Malformed)
}

/// `ddmm.mmmm` plus a hemisphere, at `index` and `index + 1`.
fn coord(fields: &[&str], index: usize) -> Result<Option<f64>, NmeaError> {
    let value = field(fields, index);
    let hemisphere = field(fields, index + 1);
    if value.is_empty() && hemisphere.is_empty() {
        return Ok(None);
    }
    // Degrees are everything left of the two minutes digits, which is two or
    // three characters depending on whether this is a latitude or a longitude.
    let point = value.find('.').ok_or(NmeaError::Malformed)?;
    let split = point.checked_sub(2).ok_or(NmeaError::Malformed)?;
    let (degrees, minutes) = value.split_at(split);
    let degrees: f64 =
        if degrees.is_empty() { 0.0 } else { degrees.parse().map_err(|_| NmeaError::Malformed)? };
    let minutes: f64 = minutes.parse().map_err(|_| NmeaError::Malformed)?;
    if !(0.0..60.0).contains(&minutes) {
        return Err(NmeaError::Malformed);
    }
    let magnitude = degrees + minutes / 60.0;
    let (limit, signed) = match hemisphere {
        "N" => (90.0, magnitude),
        "S" => (90.0, -magnitude),
        "E" => (180.0, magnitude),
        "W" => (180.0, -magnitude),
        _ => return Err(NmeaError::Malformed),
    };
    if magnitude > limit {
        return Err(NmeaError::Malformed);
    }
    Ok(Some(signed))
}

/// `ddmmyy`: a two-digit year, so a choice of century. These receivers are not from the 1900s, nor
/// is this program for the 2100s.
fn date(text: &str) -> Result<Option<NaiveDate>, NmeaError> {
    if text.is_empty() {
        return Ok(None);
    }
    let parts = (text.get(0..2), text.get(2..4), text.get(4..6));
    let (Some(day), Some(month), Some(year)) = parts else {
        return Err(NmeaError::Malformed);
    };
    let read = |t: &str| t.parse::<u32>().map_err(|_| NmeaError::Malformed);
    let (day, month, year) = (read(day)?, read(month)?, read(year)?);
    let year = i32::try_from(year).map_err(|_| NmeaError::Malformed)? + 2000;
    NaiveDate::from_ymd_opt(year, month, day).map(Some).ok_or(NmeaError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GGA: &[u8] = b"$GPGGA,123519.00,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*69\r\n";
    const GGA_NO_FIX: &[u8] = b"$GPGGA,123520.00,4807.038,N,01131.000,E,0,00,,,M,,M,,*76\r\n";
    const RMC: &[u8] = b"$GNRMC,123519.00,A,3746.4940,S,12225.1640,W,0.06,31.66,050926,,,A*74\r\n";
    const RMC_NO_FIX: &[u8] = b"$GNRMC,123519.00,V,,,,,,,050926,,,N*66\r\n";
    const GSV: &[u8] = b"$GPGSV,3,1,11,10,63,137,17,08,49,090,26,05,45,300,22,02,25,246,20*74\r\n";

    fn fix(line: &[u8]) -> GpsFix {
        match Nmea::new().parse(line) {
            Ok(Report::Fix(fix)) => fix,
            other => panic!("expected a fix, got {other:?}"),
        }
    }

    #[test]
    fn nmea_reads_position_and_altitude_when_gga_has_fix() {
        let fix = fix(GGA);
        assert!((fix.lat - 48.117_3).abs() < 1e-4, "{}", fix.lat);
        assert!((fix.lon - 11.516_666).abs() < 1e-4, "{}", fix.lon);
        assert_eq!(fix.alt, Some(545.4));
        assert_eq!(fix.satellites, Some(8));
        // 0.9 HDOP against a nominal 5 m ranging error.
        assert_eq!(fix.accuracy, Some(4.5));
    }

    #[test]
    fn nmea_negates_coordinates_when_hemisphere_south_or_west() {
        // The single most consequential thing this parser can get wrong: a
        // sign error puts every observation on the wrong side of the planet
        // and WiGLE will happily accept it.
        let fix = fix(RMC);
        assert!((fix.lat - -37.774_9).abs() < 1e-4, "{}", fix.lat);
        assert!((fix.lon - -122.419_4).abs() < 1e-4, "{}", fix.lon);
    }

    #[test]
    fn nmea_reports_no_fix_when_receiver_searching() {
        assert_eq!(Nmea::new().parse(GGA_NO_FIX), Ok(Report::NoFix));
        assert_eq!(Nmea::new().parse(RMC_NO_FIX), Ok(Report::NoFix));
    }

    #[test]
    fn nmea_reports_other_when_sentence_type_unused() {
        // A receiver emits far more GSV and GSA than GGA. Counting those as
        // failures would make a working GPS look broken.
        assert_eq!(Nmea::new().parse(GSV), Ok(Report::Other));
    }

    /// `body` framed as a sentence, with its checksum.
    fn sentence(body: &str) -> Vec<u8> {
        let sum = body.bytes().fold(0u8, |acc, b| acc ^ b);
        format!("${body}*{sum:02X}").into_bytes()
    }

    /// Parse each body in turn, asserting each is a valid sentence.
    fn feed(nmea: &mut Nmea, bodies: &[&str]) {
        for body in bodies {
            assert!(nmea.parse(&sentence(body)).is_ok(), "{body}");
        }
    }

    /// `(system, prn, snr, used)` for each satellite in view.
    fn sats(nmea: &Nmea) -> Vec<(Constellation, u16, Option<u8>, bool)> {
        nmea.sky().satellites.iter().map(|s| (s.system, s.prn, s.snr, s.used)).collect()
    }

    #[test]
    fn nmea_assembles_satellites_when_gsv_spans_messages() {
        let mut nmea = Nmea::new();
        assert_eq!(nmea.parse(GSV), Ok(Report::Other));
        assert!(nmea.sky().satellites.is_empty(), "nothing until the group's last message");
        feed(
            &mut nmea,
            &[
                "GPGSV,3,2,11,13,06,292,,15,30,050,47,18,22,091,32,21,12,033,28",
                "GPGSV,3,3,11,24,15,165,,26,34,248,35,29,21,210,40",
            ],
        );
        let sky = sats(&nmea);
        assert_eq!(sky.len(), 11, "{sky:?}");
        assert!(sky.contains(&(Constellation::Gps, 15, Some(47), false)));
        assert!(sky.contains(&(Constellation::Gps, 10, Some(17), false)));
        // In view, not tracked.
        assert!(sky.contains(&(Constellation::Gps, 13, None, false)));
    }

    #[test]
    fn nmea_marks_used_satellites_when_gsa_lists_prns() {
        let mut nmea = Nmea::new();
        feed(
            &mut nmea,
            &[
                "GPGSV,1,1,03,10,63,137,17,08,49,090,26,05,45,300,22,1",
                "GNGSA,A,3,10,08,,,,,,,,,,,1.5,0.9,1.2,1",
            ],
        );
        assert_eq!(
            sats(&nmea),
            [
                (Constellation::Gps, 5, Some(22), false),
                (Constellation::Gps, 8, Some(26), true),
                (Constellation::Gps, 10, Some(17), true),
            ]
        );
    }

    #[test]
    fn nmea_matches_used_by_prn_when_gsa_has_no_system_id() {
        // NMEA 4.0, as an older u-blox sends it: GNGSA with no system ID. Its GLONASS PRNs
        // start at 65, so the PRN alone says which satellite.
        let mut nmea = Nmea::new();
        feed(
            &mut nmea,
            &["GLGSV,1,1,02,65,40,100,30,66,20,200,25", "GNGSA,A,3,65,,,,,,,,,,,,1.5,0.9,1.2"],
        );
        assert_eq!(
            sats(&nmea),
            [
                (Constellation::Glonass, 65, Some(30), true),
                (Constellation::Glonass, 66, Some(25), false),
            ]
        );
    }

    #[test]
    fn nmea_keeps_separate_constellations_when_prns_collide() {
        let mut nmea = Nmea::new();
        feed(
            &mut nmea,
            &[
                "GPGSV,1,1,01,05,45,300,22,1",
                "GAGSV,1,1,01,05,30,120,33,7",
                "GNGSA,A,3,05,,,,,,,,,,,,1.5,0.9,1.2,3",
            ],
        );
        assert_eq!(
            sats(&nmea),
            [(Constellation::Gps, 5, Some(22), false), (Constellation::Galileo, 5, Some(33), true)]
        );
    }

    #[test]
    fn nmea_replaces_used_set_when_new_gsa_run_starts() {
        let mut nmea = Nmea::new();
        feed(
            &mut nmea,
            &[
                "GPGSV,1,1,02,10,63,137,17,08,49,090,26,1",
                "GLGSV,1,1,01,65,40,100,30,1",
                // One epoch: a GSA per constellation, which add up.
                "GNGSA,A,3,10,,,,,,,,,,,,1.5,0.9,1.2,1",
                "GNGSA,A,3,65,,,,,,,,,,,,1.5,0.9,1.2,2",
            ],
        );
        let used = |nmea: &Nmea| {
            nmea.sky().satellites.iter().filter(|s| s.used).map(|s| s.prn).collect::<Vec<_>>()
        };
        assert_eq!(used(&nmea), [10, 65]);
        // Another sentence ends the run, so the next GSA starts a fresh set.
        feed(&mut nmea, &["GPGSV,1,1,02,10,63,137,17,08,49,090,26,1"]);
        feed(&mut nmea, &["GNGSA,A,3,08,,,,,,,,,,,,1.5,0.9,1.2,1"]);
        assert_eq!(used(&nmea), [8]);
    }

    #[test]
    fn nmea_drops_partial_group_when_gsv_out_of_sequence() {
        let mut nmea = Nmea::new();
        feed(&mut nmea, &["GPGSV,1,1,01,01,45,300,22,1"]);
        // Message 2 of 3 lost: the group would be short, so the last whole one stands.
        feed(&mut nmea, &["GPGSV,3,1,09,02,45,300,22,03,45,300,22,04,45,300,22,05,45,300,22,1"]);
        feed(&mut nmea, &["GPGSV,3,3,09,10,45,300,22,1"]);
        assert_eq!(sats(&nmea), [(Constellation::Gps, 1, Some(22), false)]);
    }

    #[test]
    fn nmea_reports_fix_mode_when_gsa_says_2d_or_3d() {
        let mut nmea = Nmea::new();
        feed(&mut nmea, &["GNGSA,A,3,10,,,,,,,,,,,,1.5,0.9,1.2,1"]);
        assert_eq!(nmea.sky().mode, Some(FixMode::ThreeD));
        feed(&mut nmea, &["GNGSA,A,2,10,,,,,,,,,,,,1.5,0.9,1.2,1"]);
        assert_eq!(nmea.sky().mode, Some(FixMode::TwoD));

        // The attached u-blox, indoors: every field empty, and still unambiguous.
        let mut nmea = Nmea::new();
        for line in [
            &b"$GNRMC,,V,,,,,,,,,,N,V*37"[..],
            b"$GNVTG,,,,,,,,,N*2E",
            b"$GNGGA,,,,,,0,00,99.99,,,,,,*56",
            b"$GNGSA,A,1,,,,,,,,,,,,,99.99,99.99,99.99,1*33",
            b"$GNGSA,A,1,,,,,,,,,,,,,99.99,99.99,99.99,2*30",
            b"$GPGSV,1,1,00,1*64",
            b"$GLGSV,1,1,00,1*78",
            b"$GAGSV,1,1,00,7*73",
            b"$GBGSV,1,1,00,1*76",
            b"$GNGLL,,,,,,V,N*7A",
        ] {
            assert!(nmea.parse(line).is_ok(), "{}", String::from_utf8_lossy(line));
        }
        let sky = nmea.sky();
        assert_eq!(sky.mode, Some(FixMode::NoFix));
        assert_eq!(sky.quality, Some(0));
        assert_eq!(sky.used_count, Some(0));
        assert_eq!((sky.lat, sky.lon, sky.hdop, sky.speed_kmh), (None, None, None, None));
        assert!(sky.satellites.is_empty());
    }

    #[test]
    fn nmea_reads_speed_when_rmc_has_fix() {
        let mut nmea = Nmea::new();
        feed(&mut nmea, &["GNRMC,123519.00,A,4807.038,N,01131.000,E,10.0,31.66,050926,,,A"]);
        let sky = nmea.sky();
        assert!((sky.speed_kmh.expect("a speed") - 18.52).abs() < 1e-9);
        assert_eq!(sky.time, NaiveTime::from_hms_opt(12, 35, 19));
        assert_eq!(sky.date, NaiveDate::from_ymd_opt(2026, 9, 5));
        assert_eq!(nmea.parse(RMC_NO_FIX), Ok(Report::NoFix));
        assert_eq!(nmea.sky().speed_kmh, None);
    }

    #[test]
    fn nmea_stamps_fix_only_when_rmc_date_seen() {
        let mut nmea = Nmea::new();
        // GGA has a time of day and no date, so on its own it cannot say when.
        let Ok(Report::Fix(before)) = nmea.parse(GGA) else { panic!("a fix") };
        assert_eq!(before.at_ms, None);

        // An RMC with no fix at all still carries the date.
        assert_eq!(nmea.parse(RMC_NO_FIX), Ok(Report::NoFix));
        let Ok(Report::Fix(after)) = nmea.parse(GGA) else { panic!("a fix") };
        // 2026-09-05T12:35:19Z.
        assert_eq!(after.at_ms, Some(1_788_611_719_000));
    }

    #[test]
    fn nmea_rolls_date_forward_when_gga_crosses_midnight() {
        // GGA has a time and no date, so for the fraction of a second between
        // midnight and that cycle's RMC it borrows a date that has just
        // expired. Left alone the fix would claim to be a day old, which is
        // exactly the staleness this field exists to expose.
        let mut nmea = Nmea::new();
        let before = b"$GNRMC,235959.00,A,4807.038,N,01131.000,E,0.06,31.66,050926,,,A*7F";
        let after = b"$GPGGA,000001.00,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*65";
        let Ok(Report::Fix(_)) = nmea.parse(before) else { panic!("a fix") };
        let Ok(Report::Fix(fix)) = nmea.parse(after) else { panic!("a fix") };
        // 2026-09-06T00:00:01Z — the next day, not the previous one.
        assert_eq!(fix.at_ms, Some(1_788_652_801_000));
    }

    #[test]
    fn nmea_rejects_sentence_when_truncated_or_checksum_wrong() {
        // This is the normal first read after opening a port, and the whole
        // reason the checksum is mandatory: the prefix below is a perfectly
        // well-formed position off the coast of Somalia.
        assert_eq!(
            Nmea::new().parse(b"7.038,N,01131.000,E,1,08,0.9,545.4,M,,*69"),
            Err(NmeaError::NotASentence)
        );
        assert_eq!(Nmea::new().parse(b"$GPGGA,123519.00,4807.038,N"), Err(NmeaError::Checksum));
        assert_eq!(
            Nmea::new()
                .parse(b"$GPGGA,123519.00,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*68"),
            Err(NmeaError::Checksum)
        );
    }

    #[test]
    fn nmea_rejects_sentence_when_numeric_field_malformed() {
        let bad_hdop = b"$GPGGA,123521.00,4807.038,N,01131.000,E,1,08,zz,545.4,M,46.9,M,,*45";
        assert_eq!(Nmea::new().parse(bad_hdop), Err(NmeaError::Malformed));
        // 67 minutes of arc is not a coordinate, however well it checksums.
        let bad_minutes = b"$GPGGA,123521.00,4867.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*64";
        assert_eq!(Nmea::new().parse(bad_minutes), Err(NmeaError::Malformed));
    }
}
