//! Reading a position out of an NMEA 0183 stream.
//!
//! A deliberately small parser: two sentence types, because those two carry
//! everything the store has a column for. `GGA` has the altitude, the satellite
//! count and the dilution of precision; `RMC` has the date, without which a
//! timestamp cannot be built at all. Everything else a receiver emits — `GSV`,
//! `GSA`, `VTG`, proprietary `PMTK` chatter — parses as valid and unused.
//!
//! **The checksum is mandatory.** It is not there to catch line noise on a USB
//! CDC endpoint that already has CRCs; it is there because the first read after
//! opening a port lands mid-sentence, and half of `$GPGGA,123519,4807.038,N,…`
//! is a well-formed sentence describing somewhere else entirely.

use std::str::from_utf8;

use chrono::NaiveDate;

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
    /// [`Report::Other`] because it is the difference between "the GPS is
    /// searching" and "the GPS is talking about satellites".
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
    /// Metres above mean sea level, when the sentence carried it.
    pub alt: Option<f64>,
    /// Estimated horizontal accuracy in metres. See [`accuracy_from_hdop`].
    pub accuracy: Option<f64>,
    /// Satellites used in the solution, when reported.
    pub satellites: Option<u8>,
    /// UTC of the fix in Unix milliseconds, when the date is known.
    ///
    /// `GGA` carries a time but no date, so this stays `None` until an `RMC`
    /// has been seen. A fix is perfectly usable without it — this is what makes
    /// the receiver's own clock visible rather than silently replaced by the
    /// host's.
    pub at_ms: Option<i64>,
}

/// Horizontal dilution of precision is a multiplier, not a distance: it says
/// how much the satellite geometry amplifies ranging error. Multiplying by a
/// nominal 5 m user-equivalent range error is the usual way to get metres out
/// of it, and it is an estimate — WiGLE's `AccuracyMeters` column wants a
/// number, and an honest estimate beats the 0 that means "unknown".
#[must_use]
pub fn accuracy_from_hdop(hdop: f64) -> f64 {
    hdop * 5.0
}

/// Milliseconds in a day, and in the half day that separates "the clock has
/// wrapped past midnight" from "this receiver is confused".
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const HALF_DAY_MS: i64 = DAY_MS / 2;

/// A running parse of one receiver's output.
///
/// Holds only the date, which arrives in `RMC` and is needed to stamp the `GGA`
/// sentences between them.
#[derive(Debug, Clone, Default)]
pub struct Nmea {
    date: Option<NaiveDate>,
    last_stamp_ms: Option<i64>,
}

impl Nmea {
    /// A parser that has not yet seen a date.
    #[must_use]
    pub const fn new() -> Self {
        Self { date: None, last_stamp_ms: None }
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
        // Talker IDs vary by constellation — GP, GN, GL, BD — and a receiver
        // changes its own as satellites come and go, so only the last three
        // characters decide what a sentence is.
        let kind = id.get(id.len().saturating_sub(3)..).ok_or(NmeaError::NotASentence)?;
        let fields: Vec<&str> = fields.collect();
        match kind {
            "GGA" => self.gga(&fields),
            "RMC" => self.rmc(&fields),
            _ => Ok(Report::Other),
        }
    }

    /// `$--GGA,time,lat,N,lon,E,quality,sats,hdop,alt,M,…`
    fn gga(&mut self, f: &[&str]) -> Result<Report, NmeaError> {
        let time = field(f, 0);
        // Quality 0 is "fix not available"; 1 is GPS, 2 differential, and the
        // higher values are RTK and dead reckoning. Anything non-zero is a
        // position the receiver stands behind.
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
        // The date is worth keeping even from a sentence that carries no fix:
        // a receiver with the time but not yet a position is the normal state
        // for the first half-minute after a cold start.
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
        // A `GGA` borrows the date from the `RMC` before it, so the sentences
        // between midnight and that cycle's `RMC` borrow yesterday's and land a
        // day in the past. Time only ever runs backwards by hours for that one
        // reason; a receiver does not otherwise revisit this morning.
        let at_ms = match self.last_stamp_ms {
            Some(last) if at_ms < last - HALF_DAY_MS => at_ms + DAY_MS,
            _ => at_ms,
        };
        self.last_stamp_ms = Some(at_ms);
        Some(at_ms)
    }
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

/// A field by index, or the empty string. NMEA omits what it does not know by
/// leaving the field empty, and a short sentence is the same thing said by
/// stopping early.
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

/// `ddmmyy`, which is a two-digit year and therefore a choice about centuries.
/// These receivers are not from the 1900s and this program is not for the
/// 2100s.
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
    fn a_gga_carries_a_position_an_altitude_and_a_quality_estimate() {
        let fix = fix(GGA);
        assert!((fix.lat - 48.117_3).abs() < 1e-4, "{}", fix.lat);
        assert!((fix.lon - 11.516_666).abs() < 1e-4, "{}", fix.lon);
        assert_eq!(fix.alt, Some(545.4));
        assert_eq!(fix.satellites, Some(8));
        // 0.9 HDOP against a nominal 5 m ranging error.
        assert_eq!(fix.accuracy, Some(4.5));
    }

    #[test]
    fn the_southern_and_western_hemispheres_are_negative() {
        // The single most consequential thing this parser can get wrong: a
        // sign error puts every observation on the wrong side of the planet
        // and WiGLE will happily accept it.
        let fix = fix(RMC);
        assert!((fix.lat - -37.774_9).abs() < 1e-4, "{}", fix.lat);
        assert!((fix.lon - -122.419_4).abs() < 1e-4, "{}", fix.lon);
    }

    #[test]
    fn a_receiver_that_is_still_searching_says_so_rather_than_lying() {
        assert_eq!(Nmea::new().parse(GGA_NO_FIX), Ok(Report::NoFix));
        assert_eq!(Nmea::new().parse(RMC_NO_FIX), Ok(Report::NoFix));
    }

    #[test]
    fn sentences_this_parser_has_no_use_for_are_not_errors() {
        // A receiver emits far more GSV and GSA than GGA. Counting those as
        // failures would make a working GPS look broken.
        assert_eq!(Nmea::new().parse(GSV), Ok(Report::Other));
    }

    #[test]
    fn a_time_is_only_stamped_once_a_date_has_arrived() {
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
    fn a_position_taken_after_midnight_is_not_stamped_with_yesterday() {
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
    fn half_a_sentence_is_rejected_rather_than_read_as_a_place() {
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
    fn a_field_that_should_be_a_number_and_is_not_fails_the_sentence() {
        let bad_hdop = b"$GPGGA,123521.00,4807.038,N,01131.000,E,1,08,zz,545.4,M,46.9,M,,*45";
        assert_eq!(Nmea::new().parse(bad_hdop), Err(NmeaError::Malformed));
        // 67 minutes of arc is not a coordinate, however well it checksums.
        let bad_minutes = b"$GPGGA,123521.00,4867.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*64";
        assert_eq!(Nmea::new().parse(bad_minutes), Err(NmeaError::Malformed));
    }
}
