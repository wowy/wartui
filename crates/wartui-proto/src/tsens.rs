//! Die temperatures from the chips' temperature sensors, with each chip's eFuse calibration.
//!
//! Every chip in the fleet carries a per-chip TSENS calibration in eFuse, which ESP-IDF's driver
//! subtracts from each reading (`esp_driver_tsens/src/temperature_sensor.c`, `tsens_raw − deltaT /
//! 10`). [`calibration_tenths`] reads the field as ESP-IDF's `esp_efuse_rtc_calib_get_tsens_val`
//! does.
//!
//! The P4 bridge reads its sensor itself (`firmware/p4-bridge/src/tsens.rs`), and [`p4_tsens_c`]
//! converts its code here so `cargo test` checks it. It follows ESP-IDF 5.5.4's driver: the
//! sensor's DAC holds range index 2, offset 0, so `°C = 0.4386 · out − 20.52`, less the
//! calibration. [`p4_die_temp`] encodes it, and reads code 0 as no reading: only a dead or
//! unconverted sensor gives it, since it would mean about −21 °C, below anything the board runs at.
//!
//! The C5 and C6 read through the PHY blob, whose conversion applies no calibration, so
//! [`calibrated_c`] applies it to the PHY's whole degrees (`firmware/common/src/health.rs`).
//! [`phy_die_temp`] encodes it, and decides a failed read from the PHY's own reading, before the
//! calibration moves it. Integer math, so the firmware needs no floats.

use crate::bus::DieTemp;

/// PHY readings below this are failed reads: the PHY floors one at −200 °C, and no running die
/// is below −40 °C.
pub const PHY_FAILED_BELOW_C: i16 = -40;

/// The eFuse TSENS field's bits that ESP-IDF reads: a sign at bit 8 and a magnitude below it.
const EFUSE_BITS: u16 = 0x1FF;
/// The calibration's sign bit: set for a negative offset.
const EFUSE_SIGN: u16 = 0x100;

/// The eFuse calibration in tenths of a degree, which a reading subtracts.
///
/// `efuse` is the chip's TSENS field as read; only its low nine bits count. Bit 8 is the sign (set
/// for negative) and bits 7..0 the magnitude, in tenths of a degree, as ESP-IDF's
/// `esp_efuse_rtc_calib_get_tsens_val` reads them.
#[must_use]
pub const fn calibration_tenths(efuse: u16) -> i16 {
    let efuse = efuse & EFUSE_BITS;
    let magnitude = (efuse & 0xFF) as i16;
    if efuse & EFUSE_SIGN != 0 { -magnitude } else { magnitude }
}

/// A whole-degree reading `c` less the eFuse calibration `efuse`, rounded to the nearest degree,
/// halves away from zero.
///
/// `efuse` is read as [`calibration_tenths`] reads it.
#[must_use]
pub const fn calibrated_c(c: i16, efuse: u16) -> i16 {
    let tenths = 10 * c as i32 - calibration_tenths(efuse) as i32;
    let rounded = if tenths >= 0 { (tenths + 5) / 10 } else { (tenths - 5) / 10 };
    if rounded > i16::MAX as i32 {
        i16::MAX
    } else if rounded < i16::MIN as i32 {
        i16::MIN
    } else {
        rounded as i16
    }
}

/// The die temperature in whole degrees Celsius, rounded to the nearest, for the sensor's code
/// `out` read at range offset 0 and the eFuse calibration `efuse`.
///
/// `efuse` is the `TEMPERATURE_SENSOR` field as read, taken as [`calibration_tenths`] takes it.
/// `out` tops out at 255, about 91 °C.
#[must_use]
pub const fn p4_tsens_c(out: u8, efuse: u16) -> i16 {
    let delta_tenths = calibration_tenths(efuse) as i32;
    // In units of 1e-4 °C: 0.4386 · out − 20.52 − deltaT / 10.
    let scaled = 4386 * out as i32 - 205_200 - 1000 * delta_tenths;
    let rounded = if scaled >= 0 { (scaled + 5000) / 10_000 } else { (scaled - 5000) / 10_000 };
    rounded as i16
}

/// The P4's die temperature for the sensor's code `out` and the eFuse calibration `efuse`, as
/// [`p4_tsens_c`] converts it, or no reading for code 0.
#[must_use]
pub const fn p4_die_temp(out: u8, efuse: u16) -> DieTemp {
    if out == 0 { DieTemp::NONE } else { DieTemp::from_c(p4_tsens_c(out, efuse)) }
}

/// A C5's or C6's die temperature for the PHY's whole-degree reading `c` and the eFuse
/// calibration `efuse`, as [`calibrated_c`] corrects it, or no reading for a failed read
/// ([`PHY_FAILED_BELOW_C`]). The cutoff is the PHY's reading's, so a calibration never turns a
/// cold reading into a failed one; [`DieTemp::from_c`] clamps what it makes.
#[must_use]
pub const fn phy_die_temp(c: i16, efuse: u16) -> DieTemp {
    if c < PHY_FAILED_BELOW_C { DieTemp::NONE } else { DieTemp::from_c(calibrated_c(c, efuse)) }
}

#[cfg(test)]
mod tests {
    use super::{calibrated_c, calibration_tenths, p4_die_temp, p4_tsens_c, phy_die_temp};
    use crate::bus::DieTemp;

    // Expected values from ESP-IDF's float formula, 0.4386 · out − 20.52 − deltaT / 10,
    // rounded to the nearest degree.

    #[test]
    fn p4_tsens_converts_code_when_uncalibrated() {
        // 43.86 − 20.52 = 23.34.
        assert_eq!(p4_tsens_c(100, 0), 23);
        // 0 − 20.52.
        assert_eq!(p4_tsens_c(0, 0), -21);
        // 111.843 − 20.52 = 91.323, the top of the code.
        assert_eq!(p4_tsens_c(255, 0), 91);
        // 65.79 − 20.52 = 45.27.
        assert_eq!(p4_tsens_c(150, 0), 45);
    }

    #[test]
    fn p4_tsens_subtracts_calibration_when_efuse_positive() {
        // 23.34 − 2.5 = 20.84.
        assert_eq!(p4_tsens_c(100, 25), 21);
    }

    #[test]
    fn p4_tsens_adds_calibration_when_efuse_sign_set() {
        // 23.34 + 2.5 = 25.84.
        assert_eq!(p4_tsens_c(100, 0x100 | 25), 26);
        // 23.34 + 25.5 = 48.84, the largest magnitude.
        assert_eq!(p4_tsens_c(100, 0x1FF), 49);
    }

    #[test]
    fn p4_tsens_ignores_bits_above_nine_when_efuse_wider() {
        // esp-hal's field is ten bits wide; ESP-IDF reads nine.
        assert_eq!(p4_tsens_c(100, 0x200 | 25), p4_tsens_c(100, 25));
    }

    #[test]
    fn p4_tsens_rounds_to_nearest_when_below_zero() {
        // 0.4386 · 35 − 20.52 = −5.169.
        assert_eq!(p4_tsens_c(35, 0), -5);
        // 0.4386 · 10 − 20.52 + 0.5 = −15.634.
        assert_eq!(p4_tsens_c(10, 0x100 | 5), -16);
    }

    #[test]
    fn calibration_tenths_reads_sign_and_magnitude_when_efuse_set() {
        // The bench C6's field: −13.9 °C.
        assert_eq!(calibration_tenths(0x18b), -139);
        // The bench P4's: +7.3 °C.
        assert_eq!(calibration_tenths(0x049), 73);
        assert_eq!(calibration_tenths(0x1FF), -255);
        // A set sign over a zero magnitude is no calibration.
        assert_eq!(calibration_tenths(0x100), 0);
    }

    #[test]
    fn calibrated_adds_calibration_when_efuse_negative() {
        // The bench C6: the PHY read 33 °C, an IR thermometer about 47 °C. 33 + 13.9 = 46.9.
        assert_eq!(calibrated_c(33, 0x18b), 47);
    }

    #[test]
    fn calibrated_subtracts_calibration_when_efuse_positive() {
        // 40 − 7.3 = 32.7.
        assert_eq!(calibrated_c(40, 0x049), 33);
    }

    #[test]
    fn calibrated_keeps_reading_when_efuse_zero() {
        assert_eq!(calibrated_c(33, 0), 33);
        assert_eq!(calibrated_c(-12, 0), -12);
    }

    #[test]
    fn calibrated_ignores_bits_above_nine_when_efuse_wider() {
        assert_eq!(calibrated_c(40, 0x200 | 0x049), calibrated_c(40, 0x049));
    }

    #[test]
    fn calibrated_rounds_away_from_zero_when_result_negative() {
        // −5 − 2.5 = −7.5.
        assert_eq!(calibrated_c(-5, 25), -8);
        // −5 + 1.2 = −3.8.
        assert_eq!(calibrated_c(-5, 0x100 | 12), -4);
        // −5 + 0.5 = −4.5.
        assert_eq!(calibrated_c(-5, 0x100 | 5), -5);
        // 0 − 0.4 = −0.4.
        assert_eq!(calibrated_c(0, 4), 0);
    }

    #[test]
    fn p4_die_temp_reads_no_reading_when_code_zero() {
        assert_eq!(p4_die_temp(0, 0), DieTemp::NONE);
        assert_eq!(p4_die_temp(0, 0x049), DieTemp::NONE);
    }

    #[test]
    fn p4_die_temp_encodes_conversion_when_code_nonzero() {
        assert_eq!(p4_die_temp(150, 0).c(), Some(45));
        // 0.4386 − 20.52, about −20 °C: low, and still a reading.
        assert_eq!(p4_die_temp(1, 0).c(), Some(-20));
    }

    #[test]
    fn phy_die_temp_reads_no_reading_when_phy_read_failed() {
        // The PHY's floor for a failed read.
        assert_eq!(phy_die_temp(-200, 0x049), DieTemp::NONE);
        assert_eq!(phy_die_temp(-41, 0), DieTemp::NONE);
    }

    #[test]
    fn phy_die_temp_keeps_reading_when_calibration_takes_it_below_cutoff() {
        // −38 − 7.3 = −45.3: below −40 °C once calibrated, but the PHY read it, so it is a
        // reading, clamped to the code's floor.
        assert_eq!(calibrated_c(-38, 0x049), -45);
        assert_eq!(phy_die_temp(-38, 0x049), DieTemp::from_c(-45));
        assert_eq!(phy_die_temp(-38, 0x049).c(), Some(-40));
    }

    #[test]
    fn phy_die_temp_calibrates_when_reading_valid() {
        assert_eq!(phy_die_temp(33, 0x18b).c(), Some(47));
        assert_eq!(phy_die_temp(-40, 0).c(), Some(-40));
    }
}
