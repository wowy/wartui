//! The T-Dongle-C5's screen.
//!
//! A LilyGO T-Dongle-C5 is the same ESP32-C5 this firmware already builds for, in a
//! USB-A shell with an ST7735 160x80 panel on SPI2. This module drives it and nothing
//! else: it is handed finished lines and blits them.
//!
//! **It never composes a line.** The host does, from a snapshot, in
//! `crates/wartui-core/src/panel.rs` — which is why a change to what the panel says
//! costs a `cargo run` rather than a reflash, and why the bridge stays as ignorant of
//! what the bytes mean as the rest of this crate. What is decided here is only what
//! cannot be: the three colours a [`Severity`] means, which are a property of this
//! panel rather than of the fleet, and the fallback screen, which is link-local state
//! the host could not know.
//!
//! # Get the colour order wrong and red renders blue
//!
//! The vendor's `setRotation` ORs `0x08` into MADCTL on every rotation, so this panel
//! is BGR. On a screen whose whole job is red against green against yellow that is not
//! a cosmetic default to inherit, so [`ColorOrder::Bgr`] is set explicitly. The same
//! goes for the rest of the init list, none of which any pinout carries and all of
//! which is the difference between a working screen and a shifted, wrong-coloured one:
//! inversion on, a `(1, 26)` display offset (the vendor's `x_gap`/`y_gap` swap in
//! landscape), and rotation 3 for 160x80 the way round its own examples use.
//!
//! # Why it cannot steal time from the radio
//!
//! One line is 160 x 10 x 2 = 3.2 KB, about 1.3 ms of blocking transfer at 20 MHz, and
//! a full repaint eight of those. Three things keep that off the loop's critical path:
//! only lines whose text *or* severity changed are redrawn, the whole of it is behind
//! a floor of [`MIN_REDRAW_MS`], and the pixels go out through a `StaticCell` buffer
//! rather than the heap — `heap_allocator!` hands that to the radio blobs, and nothing
//! in wartui's own bridge code allocates.
//!
//! # The backlight is active low
//!
//! `examples/Factory/Factory.ino` toggles it `lcd_on ? 0 : 1`. Driving GPIO0 high
//! leaves the screen dark, which is exactly the failure that reads as a dead panel.

use core::fmt::Write;

use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::mono_font::ascii::FONT_6X10;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::Rectangle;
use embedded_graphics::text::{Baseline, Text};
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::Blocking;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::peripherals::{GPIO0, GPIO1, GPIO2, GPIO3, GPIO6, GPIO10, GPIO23, SPI2};
use esp_hal::spi::Mode;
use esp_hal::spi::master::{Config, Spi};
use esp_hal::time::Rate;
use mipidsi::interface::SpiInterface;
use mipidsi::models::ST7735s;
use mipidsi::options::{ColorInversion, ColorOrder, Orientation, Rotation};
use mipidsi::{Builder, Display};
use static_cell::ConstStaticCell;
use wartui_proto::link::{PANEL_ROWS, PanelLine, PanelLines, Severity, ShortStr};

use crate::Bridge;

/// Pixels across, in the landscape orientation this panel is used in.
const WIDTH: u16 = 160;

/// Pixels down.
const HEIGHT: u16 = 80;

/// Height of one row, which is `FONT_6X10`'s.
const ROW_HEIGHT: u16 = 10;

/// Characters that fit across one line: 160 pixels at six each.
pub const COLS: u8 = (WIDTH / 6) as u8;

/// Rows that fit down the screen, which is exactly what the wire carries.
pub const ROWS: u8 = PANEL_ROWS as u8;

/// The panel's SPI clock, from the vendor driver's `SPISettings(20000000, ...)`.
///
/// Not to be confused with the 40 MHz in the boot log, which is the QSPI link to the
/// flash chip on a different peripheral and different pins. Whether this panel
/// tolerates faster is untested and does not matter at one redraw a second.
const SPI_HZ: u32 = 20_000_000;

/// The shortest gap between two redraws.
///
/// The host pushes at 1 Hz and this is only a guard against one that is chatty or
/// wedged. Set below that rate on purpose: a floor at or above 1 Hz would let ordinary
/// jitter turn a one-second push into a two-second one.
const MIN_REDRAW_MS: u64 = 500;

/// How long the host may go quiet before the panel says so.
///
/// Several missed pushes rather than one. A host at 1 Hz must never flicker back to
/// the fallback between two good seconds, and the cost of waiting is only that a
/// screen is briefly out of date rather than briefly wrong.
const HOST_GONE_MS: u64 = 5_000;

/// Bytes the display interface batches pixels through.
///
/// One full row, so a cleared line is a single transfer rather than a hundred.
/// `static` rather than heap for the reason the module doc gives.
const TRANSFER_BYTES: usize = WIDTH as usize * ROW_HEIGHT as usize * 2;

/// Green: fine.
///
/// Picked for legibility on this IPS panel rather than taken as a pure primary — a
/// fully saturated green on a backlit 0.96" screen reads as a glare rather than a
/// colour, and the point is to be readable at arm's length in a car.
const OK: Rgb565 = Rgb565::new(6, 54, 10);

/// Amber: working, but not as it should be.
const WARN: Rgb565 = Rgb565::new(31, 40, 0);

/// Red: a fault, and the capture is the worse for it.
const ERROR: Rgb565 = Rgb565::new(31, 10, 8);

/// The panel's own background.
const BACKGROUND: Rgb565 = Rgb565::BLACK;

/// `ConstStaticCell` rather than `StaticCell`: the latter's `init` builds the array
/// as a value and copies it in, which puts a whole row of pixels on the stack on the
/// way to a static. This one is the array, already where it will live.
static TRANSFER: ConstStaticCell<[u8; TRANSFER_BYTES]> =
    ConstStaticCell::new([0u8; TRANSFER_BYTES]);

type Bus = ExclusiveDevice<Spi<'static, Blocking>, Output<'static>, Delay>;
type Wire = SpiInterface<'static, Bus, Output<'static>>;
type Panel = Display<Wire, ST7735s, Output<'static>>;

/// The pins the panel and the card slot share, so one call takes them all.
pub struct Pins {
    /// SPI2, which nothing else in this firmware uses.
    pub spi: SPI2<'static>,
    /// `LCD_MOSI`, shared with `SD_CMD`.
    pub mosi: GPIO2<'static>,
    /// `LCD_SCK`, shared with `SD_CLK`.
    pub sck: GPIO6<'static>,
    /// `LCD_CS`.
    pub cs: GPIO10<'static>,
    /// `LCD_DC`, which the vendor calls `LCD_RS`.
    pub dc: GPIO3<'static>,
    /// `LCD_RST`.
    pub rst: GPIO1<'static>,
    /// `LCD_BL`, active low.
    pub backlight: GPIO0<'static>,
    /// `SD_CS`, held high so the card slot stays off a bus it shares.
    pub sd_cs: GPIO23<'static>,
}

/// The panel, and what is currently drawn on it.
pub struct Screen {
    display: Panel,
    /// What each row shows now, so only what changed is redrawn.
    shown: [Option<PanelLine>; PANEL_ROWS],
    /// Whether the fallback screen is what is on there.
    ///
    /// Crossing into or out of it repaints every row rather than the dirty ones: the
    /// whole screen changes meaning, and a half-updated one would be a lie in the
    /// shape of a fact.
    fallback: bool,
    /// When the last row went out.
    last_ms: u64,
    /// Kept alive so nothing else can claim the pins, and so the screen stays lit.
    _backlight: Output<'static>,
    _sd_cs: Output<'static>,
}

impl Screen {
    /// Bring the panel up, dark screen first.
    ///
    /// Out of `main` because `main` already sits at `.clippy.toml`'s stack threshold
    /// on the C5, and `clippy::large_stack_frames` is denied.
    ///
    /// # Panics
    /// If SPI2 refuses the configuration, or the panel refuses its init sequence.
    /// Either is a wiring or a build fault rather than a condition to recover from,
    /// and the panic handler reboots.
    #[must_use]
    pub fn new(pins: Pins) -> Self {
        // Before a single byte goes down a bus the card slot shares. Nothing here
        // talks to the slot, so an `ExclusiveDevice` for the panel is correct as long
        // as that stays true — adding TF later makes it wrong and wants a shared-bus
        // device instead.
        let sd_cs = Output::new(pins.sd_cs, Level::High, OutputConfig::default());
        // Active low: `Level::Low` is lit. See the module doc.
        let backlight = Output::new(pins.backlight, Level::Low, OutputConfig::default());

        let config = Config::default()
            .with_frequency(Rate::from_hz(SPI_HZ))
            .with_mode(Mode::_0)
            .with_write_bit_order(esp_hal::spi::BitOrder::MsbFirst);
        let spi = Spi::new(pins.spi, config)
            .expect("SPI2 takes the panel's configuration")
            .with_sck(pins.sck)
            .with_mosi(pins.mosi);
        // The panel never reads, so MISO is left unclaimed even though the board wires
        // it: GPIO7 is `SD_DAT0` and belongs to the slot.

        let cs = Output::new(pins.cs, Level::High, OutputConfig::default());
        let dc = Output::new(pins.dc, Level::Low, OutputConfig::default());
        let reset = Output::new(pins.rst, Level::High, OutputConfig::default());

        let mut delay = Delay::new();
        let device = ExclusiveDevice::new(spi, cs, delay).expect("a fresh CS pin");
        let wire = SpiInterface::new(device, dc, TRANSFER.take());

        let mut display = Builder::new(ST7735s, wire)
            .reset_pin(reset)
            .display_size(WIDTH, HEIGHT)
            // The vendor's `x_gap = 26`, `y_gap = 1`, which swap in landscape.
            .display_offset(1, 26)
            // `INVON` is in the init list, with `INVOFF` commented out beside it.
            .invert_colors(ColorInversion::Inverted)
            // Not a default to inherit. See the module doc.
            .color_order(ColorOrder::Bgr)
            .orientation(Orientation::new().rotate(Rotation::Deg270))
            .init(&mut delay)
            .expect("the panel takes its init sequence");
        display.clear(BACKGROUND).expect("a cleared panel");

        Self {
            display,
            shown: [const { None }; PANEL_ROWS],
            fallback: false,
            last_ms: 0,
            _backlight: backlight,
            _sd_cs: sd_cs,
        }
    }

    /// Bring the screen up to date, and say whether anything was drawn.
    ///
    /// The return value is folded into the loop's `worked`, so a panel with nothing to
    /// say does not keep an idle bridge awake.
    pub fn render(&mut self, bridge: &mut Bridge, mac: [u8; 6]) -> bool {
        let now_ms = bridge.now_ms();
        if now_ms.saturating_sub(self.last_ms) < MIN_REDRAW_MS {
            return false;
        }

        // Reusing the clock `StallWatch` already keeps, rather than starting a second
        // one that would disagree with it: it is set by every frame that decodes, which
        // is the same evidence of a host this needs.
        let host_here =
            bridge.stall.last_host().is_some_and(|at| now_ms.saturating_sub(at) < HOST_GONE_MS);

        let (lines, fallback) = if host_here && !bridge.panel_lines.is_empty() {
            (bridge.panel_lines.clone(), false)
        } else {
            (self.own_report(bridge, mac, now_ms), true)
        };

        let crossed = self.fallback != fallback;
        self.fallback = fallback;
        let mut drew = false;
        for row in 0..PANEL_ROWS {
            let wanted = lines.get(row);
            // Severity as well as text: a line whose number held while its colour
            // changed is the whole point of having colours.
            if !crossed && self.shown[row].as_ref() == wanted {
                continue;
            }
            self.draw_row(row, wanted);
            self.shown[row] = wanted.cloned();
            drew = true;
        }

        if drew {
            self.last_ms = now_ms;
        }
        drew
    }

    /// What the bridge knows without being told, for when nobody is telling it.
    ///
    /// Link-local state only — chip, address, channel, uptime — so this costs the
    /// format-blind rule nothing. Every line is [`Severity::Warn`], because no host is
    /// exactly "working but not ideal".
    fn own_report(&self, bridge: &Bridge, mac: [u8; 6], now_ms: u64) -> PanelLines {
        let mut lines = PanelLines::new();
        let mut push = |text: ShortStr| {
            lines.push(PanelLine { level: Severity::Warn, text }).ok();
        };

        let mut who = ShortStr::new();
        let _ = write!(who, "{:?} {:02X}:{:02X}", crate::CHIP, mac[4], mac[5]);
        push(who);

        push(ShortStr::try_from("no host").unwrap_or_default());

        let mut channel = ShortStr::new();
        let _ = write!(channel, "channel {}", bridge.channel);
        push(channel);

        let mut uptime = ShortStr::new();
        let seconds = now_ms / 1_000;
        let _ = write!(
            uptime,
            "up {}h{:02}m{:02}s",
            seconds / 3_600,
            (seconds / 60) % 60,
            seconds % 60
        );
        push(uptime);

        lines
    }

    /// Repaint one row: clear its strip, then draw whatever belongs on it.
    ///
    /// The clear is what makes a shorter line replace a longer one rather than wearing
    /// its tail, and it is one transfer because the buffer is a whole row wide.
    fn draw_row(&mut self, row: usize, line: Option<&PanelLine>) {
        let top = row as u16 * ROW_HEIGHT;
        let strip = Rectangle::new(
            Point::new(0, i32::from(top)),
            Size::new(u32::from(WIDTH), u32::from(ROW_HEIGHT)),
        );
        // Every draw here is infallible in practice — the interface cannot report a
        // panel that is not listening — and there is nowhere to report it to anyway:
        // the screen is the report.
        let _ = self.display.fill_solid(&strip, BACKGROUND);

        let Some(line) = line else { return };
        let style = MonoTextStyle::new(&FONT_6X10, colour(line.level));
        let _ = Text::with_baseline(
            line.text.as_str(),
            Point::new(0, i32::from(top)),
            style,
            Baseline::Top,
        )
        .draw(&mut self.display);
    }
}

/// What a severity looks like on this panel.
const fn colour(level: Severity) -> Rgb565 {
    match level {
        Severity::Ok => OK,
        Severity::Warn => WARN,
        Severity::Error => ERROR,
    }
}
