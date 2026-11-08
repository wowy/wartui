//! The carrier's lid fan, a Noctua NF-A6x15 5V PWM: the duty the operator sets, the PWM wave the
//! P4 drives for it, and the tach reading.
//!
//! **The P4's PWM pin is inverted**: GPIO51 high holds the fan's PWM input low. So the fan's duty
//! is the share of each period GPIO51 is low, and [`pwm`] says how long the pin stays high. A
//! GPIO51 that is not driven, as through a P4 reset, runs the fan at full speed, so 100 % is the
//! channel stopped with the pin at its idle low ([`Pwm::Full`]): the P4 at full duty looks the
//! same as the P4 in reset. 0 % holds the pin high ([`Pwm::GpioHigh`]).
//!
//! **The fan stops at low duty.** On the bench it kept turning down to 10 % once spinning, needed
//! 15 % to start from rest, and stopped at 5 % and 0 %. So a tach reading 0 rpm is a fault only at
//! [`FanDuty::TURNS_FROM`] and above; below it the fan may simply be stopped.
//!
//! **The tach gives [`TACH_PULSES_PER_REV`] falling edges a revolution**, about 117 a second at
//! the fan's 3500 rpm. [`TachFilter`] drops edges closer than [`TACH_MIN_GAP_US`], which rejects
//! ringing on the line, and the P4 counts what it keeps over each [`TACH_WINDOW_MS`]. Until the
//! first whole window after boot, [`FanStats::rpm`] is `None`, so a fresh boot never reads as a
//! stopped fan. It is `None` again from a duty change until a window that began after it ends, so
//! no reading spans two duties.
//!
//! **The P4 keeps the duty, not the host.** It boots into the stored one, or [`FanDuty::FULL`]
//! with none, and holds it with no host present. [`crate::settings`] has the record.

use core::fmt;

use serde::{Deserialize, Serialize};

/// The PWM frequency the P4 drives. The fan takes 21 to 28 kHz.
pub const PWM_HZ: u32 = 25_000;

/// Tach pulses a revolution.
pub const TACH_PULSES_PER_REV: u32 = 2;

/// How long the P4 counts tach edges for each reading.
pub const TACH_WINDOW_MS: u32 = 1000;

/// The closest two tach edges may be and both count. It still counts up to 15 000 rpm.
pub const TACH_MIN_GAP_US: u64 = 2000;

/// A fan duty in percent, 0 to 100.
///
/// Serialized as its `u8`, so a value over 100 fails to decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct FanDuty(u8);

impl FanDuty {
    /// Full speed: the duty with nothing stored, and the fan's speed while the P4 is in reset.
    pub const FULL: Self = Self(100);

    /// The least duty the fan is sure to turn at, from rest or slowing: 15 % starts it on the
    /// bench, and 10 % keeps it turning only once it is. A 0 rpm reading below this is the duty,
    /// not a fault.
    pub const TURNS_FROM: Self = Self(20);

    /// `pct` as a duty, or `None` over 100.
    #[must_use]
    pub const fn new(pct: u8) -> Option<Self> {
        if pct > 100 { None } else { Some(Self(pct)) }
    }

    /// The duty in percent.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// A duty over 100 %.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DutyOutOfRange(pub u8);

impl fmt::Display for DutyOutOfRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fan duty {} is over 100", self.0)
    }
}

impl TryFrom<u8> for FanDuty {
    type Error = DutyOutOfRange;

    fn try_from(pct: u8) -> Result<Self, Self::Error> {
        Self::new(pct).ok_or(DutyOutOfRange(pct))
    }
}

impl From<FanDuty> for u8 {
    fn from(duty: FanDuty) -> Self {
        duty.0
    }
}

impl fmt::Display for FanDuty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}%", self.0)
    }
}

/// What the P4 drives on GPIO51 for a duty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pwm {
    /// Nothing: the channel stopped, the pin at its idle low. Full speed.
    Full,
    /// The pin held high: 0 %, which stops the fan.
    GpioHigh,
    /// One period, repeated: the pin high for `gpio_high` ticks, then low for `gpio_low`.
    Wave {
        /// Ticks high, at least 1.
        gpio_high: u16,
        /// Ticks low, at least 1.
        gpio_low: u16,
    },
}

/// The wave for `duty` over a period of `period_ticks`, at least 2.
///
/// The fan's duty is the share of the period the pin is low, so the pin is high for the rest,
/// rounded to the nearest tick. Each half keeps at least one tick.
#[must_use]
pub fn pwm(duty: FanDuty, period_ticks: u16) -> Pwm {
    match duty.get() {
        100 => Pwm::Full,
        0 => Pwm::GpioHigh,
        pct => {
            let period = u32::from(period_ticks);
            let high = (period * u32::from(100 - pct) + 50) / 100;
            let high = high.clamp(1, period.saturating_sub(1).max(1));
            let gpio_high = u16::try_from(high).unwrap_or(u16::MAX);
            Pwm::Wave { gpio_high, gpio_low: period_ticks.saturating_sub(gpio_high).max(1) }
        }
    }
}

/// Drops a tach edge closer than [`TACH_MIN_GAP_US`] to the last one kept.
#[derive(Debug, Clone, Copy, Default)]
pub struct TachFilter {
    last_us: Option<u64>,
}

impl TachFilter {
    /// A filter that has kept no edge.
    #[must_use]
    pub const fn new() -> Self {
        Self { last_us: None }
    }

    /// An edge at `at_us`: whether it counts.
    pub fn accept(&mut self, at_us: u64) -> bool {
        if let Some(last) = self.last_us
            && at_us.saturating_sub(last) < TACH_MIN_GAP_US
        {
            return false;
        }
        self.last_us = Some(at_us);
        true
    }
}

/// The speed `edges` tach edges in `window_ms` make, saturating at `u16::MAX`. 0 for an empty
/// window.
#[must_use]
pub fn rpm(edges: u32, window_ms: u32) -> u16 {
    let per_minute = u64::from(edges) * 60_000;
    let per_window = u64::from(TACH_PULSES_PER_REV) * u64::from(window_ms);
    let rpm = per_minute.checked_div(per_window).unwrap_or(0);
    u16::try_from(rpm).unwrap_or(u16::MAX)
}

/// The fan as the P4 reports it in `Status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FanStats {
    /// The duty in force.
    pub duty: FanDuty,
    /// The speed over the last whole tach window, or `None` before the first since boot or since
    /// the duty last changed.
    pub rpm: Option<u16>,
}
