//! The P4's die temperature from its temperature sensor's code.
//!
//! The P4 bridge reads the sensor itself (`firmware/p4-bridge/src/tsens.rs`), and the conversion
//! lives here so `cargo test` checks it. It follows ESP-IDF 5.5.4's driver: the sensor's DAC
//! holds range index 2, offset 0, so `°C = 0.4386 · out − 20.52`, less the chip's eFuse
//! calibration in tenths of a degree. Integer math, so the firmware needs no floats.

/// The eFuse `TEMPERATURE_SENSOR` field's bits that ESP-IDF reads: a sign at bit 8 and a
/// magnitude below it.
const EFUSE_BITS: u16 = 0x1FF;
/// The calibration's sign bit: set for a negative offset.
const EFUSE_SIGN: u16 = 0x100;

/// The die temperature in whole degrees Celsius, rounded to the nearest, for the sensor's code
/// `out` read at range offset 0 and the eFuse calibration `efuse`.
///
/// `efuse` is the `TEMPERATURE_SENSOR` field as read; only its low nine bits count. Bit 8 is
/// the sign and bits 7..0 the magnitude, in tenths of a degree, as ESP-IDF's
/// `esp_efuse_rtc_calib_get_tsens_val` reads them. `out` tops out at 255, about 91 °C.
#[must_use]
pub const fn p4_tsens_c(out: u8, efuse: u16) -> i16 {
    let efuse = efuse & EFUSE_BITS;
    let magnitude = (efuse & 0xFF) as i32;
    let delta_tenths = if efuse & EFUSE_SIGN != 0 { -magnitude } else { magnitude };
    // In units of 1e-4 °C: 0.4386 · out − 20.52 − deltaT / 10.
    let scaled = 4386 * out as i32 - 205_200 - 1000 * delta_tenths;
    let rounded = if scaled >= 0 { (scaled + 5000) / 10_000 } else { (scaled - 5000) / 10_000 };
    rounded as i16
}

#[cfg(test)]
mod tests {
    use super::p4_tsens_c;

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
}
