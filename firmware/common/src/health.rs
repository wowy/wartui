//! The chip's supply voltage and die temperature, for a node's heartbeat and the P4 bridge's
//! C6's `Up::Health`.
//!
//! Both come from the PHY blob, through functions it exports but no header declares. No
//! board in the fleet has a divider on an ADC pin, so [`vdd_mv`] calls `phy_get_vdd33`. It
//! measures the chip's 3.3 V rail, not the board's 5 V input.
//!
//! [`die_temp_c`] calls `phy_get_tsens_value`. The PHY powers the TSENS block while the
//! radio is up and owns its range: its own tracking reads move the sensor's DAC between
//! ranges as the die heats and cools, and each range converts with its own offset. A reader
//! of the registers would have to know which range the PHY last chose, so this reads
//! through the PHY instead.
//!
//! The PHY's conversion applies none of the chip's eFuse TSENS calibration, which ESP-IDF's
//! driver subtracts from every reading, so [`die_temp_calibrated_c`] applies it
//! (`wartui_proto::tsens::calibrated_c`). On the bench C6 the PHY read 33 °C while an IR
//! thermometer read about 47 °C on the package; its field read 0x18b, −13.9 °C, and calibrated
//! the reading is 47 °C. The C5 uses the same PHY call and the same eFuse scheme. The P4 reads
//! its own sensor and applies its calibration there (`firmware/p4-bridge/src/tsens.rs`), so the
//! calibrated reading is the C5's and C6's alone.

/// The chip's 3.3 V rail in millivolts, or 0 when the PHY is not initialized.
///
/// Call it only where the PHY is initialized: a node's heartbeat path, where the radio is up and
/// parked on the control channel, or a C6 whose Bluetooth controller has started. The read
/// borrows the PBUS for a moment, which a node's dwell must not lose.
#[allow(unsafe_code, reason = "the PHY blob's rail read has no esp-hal or esp-radio wrapper")]
#[must_use]
pub fn vdd_mv() -> u16 {
    unsafe extern "C" {
        fn phy_get_vdd33() -> u16;
    }

    // SAFETY: `phy_get_vdd33` is `extern "C" fn() -> u16` in both chips' PHY blobs,
    // taking no arguments and touching no memory of ours. Disassembled, it switches the
    // PBUS into debug mode, routes VDD3P3 to SAR2 over the analog I2C bus, reads SAR2,
    // restores the PBUS, and scales the reading (×206/100 on the C6, ×236/100 on the
    // C5). It returns 0 before the PHY is initialized. The unit is millivolts, reading low
    // by a per-chip offset. `crates/wartui/README.md` § "Reading the node blocks" has the
    // measurements.
    unsafe { phy_get_vdd33() }
}

/// The die temperature in whole degrees Celsius, as the PHY converts it, uncalibrated.
///
/// Call it only where the PHY is initialized, as for [`vdd_mv`]. A reading to report is
/// [`die_temp_calibrated_c`]'s.
#[allow(unsafe_code, reason = "the PHY blob's range-aware TSENS read has no esp-hal wrapper")]
#[must_use]
pub fn die_temp_c() -> i16 {
    unsafe extern "C" {
        fn phy_get_tsens_value() -> i16;
    }

    // SAFETY: `phy_get_tsens_value` takes no arguments and touches no memory of ours. The
    // C5's caller stores its result as a halfword, hence `i16`. Disassembled, it tail-calls
    // `ram_tsens_temp_read_new` on the C6 and `phy_tsens_temp_read_local` on the C5. Each
    // reads the TSENS DAC over the PHY's analog I2C (bus 0x69, register 6), maps it to a
    // range index, reads the TSENS code and converts it with that range's offset; the C5's
    // `phy_code_to_temp` is `(44·raw − 2788·offset − 2052) / 100`, clamped at −200, in
    // whole °C. Each then re-ranges the DAC, exactly as the PHY's own tracking reads do.
    // The PHY must be initialized, which each caller guarantees: a node's heartbeat path, and a
    // C6 only once its Bluetooth controller has started.
    unsafe { phy_get_tsens_value() }
}

/// The die temperature in whole degrees Celsius, less the chip's eFuse calibration: [`die_temp_c`]
/// through [`calibrate_die_temp`].
///
/// Call it only where the PHY is initialized, as for [`vdd_mv`].
#[cfg(any(feature = "esp32c5", feature = "esp32c6"))]
#[must_use]
pub fn die_temp_calibrated_c() -> i16 {
    calibrate_die_temp(die_temp_c())
}

/// A [`die_temp_c`] reading less the chip's eFuse calibration.
///
/// A reading below −40 °C, which no running die is and the PHY's −200 °C floor for a failed read
/// is, passes through uncalibrated, so a caller still reads it as failed.
#[cfg(any(feature = "esp32c5", feature = "esp32c6"))]
#[must_use]
pub fn calibrate_die_temp(c: i16) -> i16 {
    if c < -40 { c } else { wartui_proto::tsens::calibrated_c(c, tsens_calibration()) }
}

/// The chip's eFuse TSENS calibration field, as `wartui_proto::tsens::calibration_tenths` takes
/// it.
///
/// Read at each call: a few eFuse register reads, once a heartbeat or health report. The P4's
/// field is `firmware/p4-bridge/src/tsens.rs`'s to read, so this is the C5's and C6's alone.
#[cfg(any(feature = "esp32c5", feature = "esp32c6"))]
#[must_use]
pub fn tsens_calibration() -> u16 {
    #[cfg(feature = "esp32c5")]
    let field = esp_hal::efuse::TEMPERATURE_SENSOR;
    #[cfg(feature = "esp32c6")]
    let field = esp_hal::efuse::TEMP_CALIB;
    esp_hal::efuse::read_field_le::<u16>(field)
}
