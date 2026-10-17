//! The T-Dongle-C5's RGB LED, turned off.
//!
//! The board carries an APA102. It powers up showing whatever its latches hold, which on
//! this board is a steady blue, and stays that way until something writes it a frame. Nothing
//! else in this firmware does, so this module writes one frame at brightness zero and leaves
//! clock and data low for the life of `main`.
//!
//! The pins are the vendor's: `include/pin_config.h` puts `LED_CI_PIN` on GPIO4 and
//! `LED_DI_PIN` on GPIO5, and `examples/LED/led.ino` drives them as clock and data. The LED
//! belongs to the board, not to the panel or the GPS port, so either T-Dongle-C5 feature
//! blanks it.
//!
//! It stays lit from power-up until `main` runs, since the bootloader does not know it is there.

use esp_hal::delay::Delay;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::peripherals::{GPIO4, GPIO5};

/// The bits clocked into the LED, most significant first.
const FRAME: [u8; 16] = [
    // Two start frames. The clock floats through reset and boot, so stray edges can leave the
    // LED's shift register mid-frame; 64 zero bits resync it.
    0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, //
    0xE0, 0x00, 0x00, 0x00, // header 111, brightness 0, B, G, R
    // The end frame only supplies clocks. Zeros, because an SK9822 sold as an APA102 latches
    // a frame only when zero bits follow it.
    0x00, 0x00, 0x00, 0x00,
];

/// Comfortably longer than the APA102's setup and hold times.
const EDGE_NS: u32 = 500;

/// Write the LED one frame at brightness zero, from `LED_CI_PIN` (clock) and `LED_DI_PIN`
/// (data). Both lines rest low on return; hold the returned pins so the clock never floats.
pub fn darken(ci: GPIO4<'static>, di: GPIO5<'static>) -> (Output<'static>, Output<'static>) {
    let mut ci = Output::new(ci, Level::Low, OutputConfig::default());
    let mut di = Output::new(di, Level::Low, OutputConfig::default());
    let delay = Delay::new();
    for byte in FRAME {
        for bit in (0..8).rev() {
            di.set_level(Level::from(byte >> bit & 1 == 1));
            delay.delay_nanos(EDGE_NS);
            // The LED samples data on the rising edge.
            ci.set_high();
            delay.delay_nanos(EDGE_NS);
            ci.set_low();
        }
    }
    di.set_low();
    (ci, di)
}
