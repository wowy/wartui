//! A node's supply voltage and die temperature, one byte each.
//!
//! A heartbeat is sent every few seconds by every node, so each reading gets the smallest
//! encoding that still answers the operator's question: is this node browning out, and is
//! it cooking. Both are integer codes with one value set aside for a failed read, so the
//! firmware converts without floats and the host never mistakes a failure for a reading.

use core::fmt;

/// The lowest voltage [`Vdd`] encodes, below its first step.
const VDD_BASE_MV: u16 = 2500;
/// One [`Vdd`] step.
const VDD_STEP_MV: u16 = 12;

/// The chip's 3.3 V rail, in 12 mV steps above 2.5 V.
///
/// `mV = 2500 + 12 × code` for codes 1 to 255, so 2.512 V to 5.560 V. Code 0 is no
/// reading. It measures the rail the regulator feeds the chip, not the board's 5 V input,
/// so a sagging supply shows here once the regulator can no longer hold 3.3 V.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vdd(pub u8);

impl Vdd {
    /// No reading: the read failed, or ran before the radio was up.
    pub const NONE: Self = Self(0);

    /// Encode a reading in millivolts, rounding to the nearest step.
    ///
    /// 0 mV is what a failed read returns, and becomes [`Vdd::NONE`]. Anything else is
    /// clamped to the encodable range rather than refused.
    #[must_use]
    pub const fn from_mv(mv: u16) -> Self {
        if mv == 0 {
            return Self::NONE;
        }
        let above = mv.saturating_sub(VDD_BASE_MV) as u32;
        let code = (above + VDD_STEP_MV as u32 / 2) / VDD_STEP_MV as u32;
        Self(if code < 1 {
            1
        } else if code > u8::MAX as u32 {
            u8::MAX
        } else {
            code as u8
        })
    }

    /// The reading in millivolts, or `None` when there was none.
    #[must_use]
    pub const fn mv(self) -> Option<u16> {
        if self.0 == 0 { None } else { Some(VDD_BASE_MV + VDD_STEP_MV * self.0 as u16) }
    }
}

/// `3.30 V`, or `—` for no reading.
impl fmt::Display for Vdd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.mv() {
            Some(mv) => {
                let cv = (mv + 5) / 10;
                write!(f, "{}.{:02} V", cv / 100, cv % 100)
            }
            None => f.write_str("—"),
        }
    }
}

/// The lowest temperature [`DieTemp`] encodes, as code 0.
const DIE_TEMP_BASE_C: i16 = -40;

/// The die temperature in whole degrees Celsius, offset by 40.
///
/// `°C = code − 40` for codes 0 to 254, so −40 °C to 214 °C. 0xFF is no reading. The PHY
/// reads whole degrees, so a finer step would carry nothing, and the range spans every
/// temperature the chip is rated for with the die's headroom above ambient.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DieTemp(pub u8);

impl DieTemp {
    /// No reading.
    pub const NONE: Self = Self(0xFF);

    /// Encode a reading in whole degrees, clamped to −40 °C to 214 °C.
    #[must_use]
    pub const fn from_c(c: i16) -> Self {
        let code = c as i32 - DIE_TEMP_BASE_C as i32;
        Self(if code < 0 {
            0
        } else if code > 254 {
            254
        } else {
            code as u8
        })
    }

    /// The reading in whole degrees Celsius, or `None` when there was none.
    #[must_use]
    pub const fn c(self) -> Option<i16> {
        if self.0 == Self::NONE.0 { None } else { Some(self.0 as i16 + DIE_TEMP_BASE_C) }
    }
}

/// `41 °C`, or `—` for no reading.
impl fmt::Display for DieTemp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.c() {
            Some(c) => write!(f, "{c} °C"),
            None => f.write_str("—"),
        }
    }
}
