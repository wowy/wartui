//! The P4's die temperature, from its on-die temperature sensor, for `Status`.
//!
//! esp-hal 1.2.2 has no temperature-sensor driver for the P4: its `tsens` covers the C3 and C6,
//! it names no `LP_TSENS` singleton, and its PAC is private to it. So this follows ESP-IDF
//! 5.5.4's driver by hand: `hal/esp32p4/include/hal/temperature_sensor_ll.h`,
//! `hal/esp32p4/include/hal/regi2c_ctrl_ll.h` (`regi2c_ctrl_ll_i2c_sar_periph_enable`),
//! `esp_hw_support/sar_periph_ctrl_common.c` (`temperature_sensor_power_acquire`),
//! `soc/esp32p4/temperature_sensor_periph.c` (the ranges),
//! `esp_driver_tsens/src/temperature_sensor.c` (the calibration's use) and
//! `efuse/esp32p4/esp_efuse_rtc_calib.c` (its reading).
//!
//! [`init`] runs once at boot, after `esp_hal::init` and the PSRAM's mapping: it powers the
//! analog I2C toward the SAR block, clocks and resets the sensor, powers it up, sets its range
//! over the analog I2C and waits for the first conversion. [`Tsens::read`] is one register read
//! then, so `Status` reads it each time it is built.
//!
//! Two regions are `unsafe`, each in its own function, because no safe route reaches them:
//!
//! - [`steal`] takes the PAC's `LP_TSENS` block. esp-hal names no singleton for it, so nothing
//!   else can own it; the PAC is a direct dependency here for that alone.
//! - [`write_tsens_dac`] sets the range over the analog I2C. esp-hal's own analog-I2C access is
//!   `pub(crate)`, and each write to the master's control registers is a raw field value, which
//!   the PAC marks `unsafe`.
//!
//! The range is index 2 of ESP-IDF's table: DAC code 15, offset 0, rated −10 to 80 °C, the
//! smallest error (1 °C) and the range ESP-IDF starts in. It is never moved, unlike ESP-IDF's
//! driver, which re-ranges as the die heats. The 8-bit code tops out at about 91 °C at this
//! range, so a reading there means that or hotter. The conversion is
//! `wartui_proto::tsens::p4_tsens_c`, tested on the host. The bench checks the reading: it moves
//! with the lid fan on and off, and sits near the C6's own reading beside it.

use core::fmt::Write as _;

use esp_hal::delay::Delay;
use esp_hal::peripherals::{LP_I2C_ANA_MST, LP_PERI, PMU};
use wartui_proto::bus::DieTemp;
use wartui_proto::link::LogStr;
use wartui_proto::tsens::p4_tsens_c;

/// The analog I2C block of the SAR ADC and the temperature sensor.
const I2C_SAR_ADC: u8 = 0x69;
/// Its register holding `I2C_SARADC_TSENS_DAC` in bits 3..0.
const TSENS_DAC_REG: u8 = 6;
/// Bits of [`TSENS_DAC_REG`] that are the DAC.
const TSENS_DAC_MASK: u8 = 0x0F;
/// The DAC code of range index 2: offset 0, −10 to 80 °C.
const TSENS_DAC_CODE: u8 = 15;

/// `ANA_CONF2`'s bit selecting the SAR block's master, from ESP-IDF's
/// `esp_rom_regi2c_esp32p4.c` and esp-hal's `soc/esp32p4/regi2c.rs`.
const SAR_I2C_MST_SEL: u32 = 1 << 7;
/// `I2C0_CTRL`'s fields: the slave id in 7..0, the register in 15..8, the data in 23..16 and the
/// write flag at 24.
const CTRL_ADDR_SHIFT: u32 = 8;
const CTRL_DATA_SHIFT: u32 = 16;
const CTRL_WRITE: u32 = 1 << 24;

/// How long the first conversion takes after the sensor powers up and its range is set: ESP-IDF
/// waits 200 µs before a first read, and 300 µs after a range change.
const SETTLE_US: u32 = 300;

/// The powered sensor and the chip's calibration.
pub struct Tsens {
    block: esp32p4::LP_TSENS,
    /// The eFuse `TEMPERATURE_SENSOR` field as read, which `p4_tsens_c` takes nine bits of.
    calibration: u16,
}

impl Tsens {
    /// The die temperature now.
    pub fn read(&self) -> DieTemp {
        let out = self.block.ctrl().read().out().bits();
        DieTemp::from_c(p4_tsens_c(out, self.calibration))
    }

    /// Say the eFuse calibration and one reading's raw code beside what it converts to, once
    /// at boot, so a bench can check the correction by hand: `tsens: calibration 0x019, code 150,
    /// 45 °C`. The SD log keeps it, and a host attached by then sees it.
    #[inline(never)]
    pub fn say_calibration(&self, shared: &crate::Shared) {
        let out = self.block.ctrl().read().out().bits();
        let mut line = LogStr::new();
        let _ = write!(
            line,
            "tsens: calibration {:#05x}, code {out}, {}",
            self.calibration,
            DieTemp::from_c(p4_tsens_c(out, self.calibration))
        );
        shared.push_log(&line);
    }
}

/// Power the sensor, set its range and wait for its first conversion. Once, at boot, after
/// `esp_hal::init` and the PSRAM's mapping, the last of esp-hal's analog I2C writes.
pub fn init() -> Tsens {
    // The analog I2C toward the SAR block: held in reset, powered, then released, as
    // `regi2c_ctrl_ll_i2c_sar_periph_enable` does.
    let pmu = PMU::regs();
    pmu.rf_pwc().modify(|_, w| w.perif_i2c_rstb().clear_bit());
    pmu.rf_pwc().modify(|_, w| w.xpd_perif_i2c().set_bit());
    pmu.rf_pwc().modify(|_, w| w.perif_i2c_rstb().set_bit());

    // The sensor's bus clock, and a reset.
    let lp_peri = LP_PERI::regs();
    lp_peri.clk_en().modify(|_, w| w.ck_en_lp_tsens().set_bit());
    lp_peri.reset_en().modify(|_, w| w.rst_en_lp_tsens().set_bit());
    lp_peri.reset_en().modify(|_, w| w.rst_en_lp_tsens().clear_bit());

    // Powered up, with its clock divider at its reset value, which ESP-IDF leaves alone.
    let block = steal();
    block.ctrl().modify(|_, w| w.power_up().set_bit());

    write_tsens_dac(TSENS_DAC_CODE);
    Delay::new().delay_micros(SETTLE_US);

    let calibration = esp_hal::efuse::read_field_le::<u16>(esp_hal::efuse::TEMPERATURE_SENSOR);
    Tsens { block, calibration }
}

/// The PAC's `LP_TSENS` block.
#[allow(unsafe_code, reason = "esp-hal names no LP_TSENS singleton to take it from")]
fn steal() -> esp32p4::LP_TSENS {
    // SAFETY: nothing else in this image touches LP_TSENS. esp-hal has no P4 temperature-sensor
    // driver and no `LP_TSENS` singleton, and neither it, esp-rtos, esp-bootloader-esp-idf nor
    // any of this firmware's code names the block, so the one instance taken here, by `init`, once,
    // is its sole owner. It is a plain register block with no memory of ours behind it.
    unsafe { esp32p4::LP_TSENS::steal() }
}

/// Write `code` to `I2C_SARADC_TSENS_DAC`, bits 3..0 of the SAR block's register 6, over the
/// analog I2C, keeping the register's other bits: a read, then a write, as ESP-IDF's
/// `regi2c_write_mask_impl` does.
#[allow(
    unsafe_code,
    reason = "esp-hal's analog I2C access is pub(crate), and the master's fields are raw values"
)]
fn write_tsens_dac(code: u8) {
    // The master's clock, from the 160 MHz source, as esp-hal's `regi2c_enable_block` sets it.
    LP_PERI::regs().clk_en().modify(|_, w| w.ck_en_lp_i2cmst().set_bit());
    let master = LP_I2C_ANA_MST::regs();
    master.clk160m().modify(|_, w| w.clk_i2c_mst_sel_160m().set_bit());

    let wait_idle = || {
        while master.i2c0_ctrl().read().i2c0_busy().bit_is_set() {
            core::hint::spin_loop();
        }
    };
    let target = u32::from(I2C_SAR_ADC) | (u32::from(TSENS_DAC_REG) << CTRL_ADDR_SHIFT);

    // SAFETY: each `bits` writes a whole field with a value from esp-hal's own P4 sequence
    // (`soc/esp32p4/regi2c.rs`, `regi2c_enable_block`, `regi2c_read` and `regi2c_write`), which
    // matches ESP-IDF's `esp_rom_regi2c_esp32p4.c`: both master selects cleared, then the SAR
    // block's select bit alone; a read command of the block's id and register; a write command
    // of the same with the write flag and the data byte. No value reaches past its field: the
    // id, register and data are bytes in their own bit ranges, and the flag is bit 24 of the
    // 25-bit field. Nothing else uses the analog I2C while this runs. esp-hal's other users of it
    // are the clock setup in `esp_hal::init`, the MPLL's start, which only the PSRAM asks for
    // here (`Psram::new`, under `Cache::new`), and its ADC driver, which this firmware does not
    // use. The first two have finished by the time `init` calls this, once, at boot, and the P4's
    // core 1 is never started (AGENTS.md), so no other core can be mid-transaction either.
    unsafe {
        master.ana_conf2().modify(|_, w| w.ana_conf2().bits(0));
        master.ana_conf1().modify(|_, w| w.ana_conf1().bits(0));
        master
            .ana_conf2()
            .modify(|r, w| w.ana_conf2().bits(r.ana_conf2().bits() | SAR_I2C_MST_SEL));

        wait_idle();
        master.i2c0_ctrl().write(|w| w.i2c0_ctrl().bits(target));
        wait_idle();
        let read = (master.i2c0_ctrl().read().i2c0_ctrl().bits() >> CTRL_DATA_SHIFT) as u8;

        let data = (read & !TSENS_DAC_MASK) | (code & TSENS_DAC_MASK);
        let write = target | CTRL_WRITE | (u32::from(data) << CTRL_DATA_SHIFT);
        master.i2c0_ctrl().write(|w| w.i2c0_ctrl().bits(write));
        wait_idle();
    }
}
