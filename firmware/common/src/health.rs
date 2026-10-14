//! The chip's supply voltage and die temperature, for the node's heartbeat.
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

/// The chip's 3.3 V rail in millivolts, or 0 when the PHY is not initialized.
///
/// Call it only from the heartbeat path, where the radio is up and parked on the control
/// channel. The read borrows the PBUS for a moment, which a dwell must not lose.
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
    // C5). It returns 0 before the PHY is initialized. The unit is millivolts, reading
    // 50 to 100 mV below a multimeter on the board's 3V3 pin.
    unsafe { phy_get_vdd33() }
}

/// The die temperature in whole degrees Celsius.
///
/// Call it only from the heartbeat path, where the PHY is initialized and the radio is up,
/// as for [`vdd_mv`].
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
    // The PHY must be initialized, which the heartbeat path guarantees.
    unsafe { phy_get_tsens_value() }
}
