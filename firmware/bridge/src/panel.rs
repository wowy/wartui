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
//! inversion on, the vendor's `(26, 1)` offset into the controller's larger
//! framebuffer, and rotation 3 for 160x80 the way round its own examples use.
//!
//! # Why it cannot steal time from the radio
//!
//! One line is 160 x 15 x 2 = 4.8 KB of pixels and 1.9 ms of bus time, and costs about
//! five times that: 9.3 ms measured, against 36 ms for a full repaint
//! (`docs/t-dongle-c5-findings.md`). The cost is the glyphs rather than the bus.
//!
//! Four things keep that off the loop's critical path: only lines whose text *or*
//! severity changed are redrawn, drawing is behind a floor of [`MIN_REDRAW_MS`],
//! deciding whether to draw is behind [`MIN_LOOK_MS`], and the pixels go out through a
//! `StaticCell` buffer rather than the heap — `heap_allocator!` hands that to the radio
//! blobs, and nothing in wartui's own bridge code allocates.
//!
//! # The backlight is active low
//!
//! `examples/Factory/Factory.ino` toggles it `lcd_on ? 0 : 1`. Driving GPIO0 high
//! leaves the screen dark, which is exactly the failure that reads as a dead panel.

use core::fmt::Write;

use embedded_graphics::mono_font::ascii::FONT_9X15;
use embedded_graphics::mono_font::{MonoFont, MonoTextStyle};
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
use static_cell::{ConstStaticCell, StaticCell};
use wartui_proto::link::{PANEL_ROWS, PanelLine, PanelLines, Severity, ShortStr};

use crate::Bridge;

/// Pixels across, in the landscape orientation this panel is used in.
const WIDTH: u16 = 160;

/// Pixels down, in that same orientation.
const HEIGHT: u16 = 80;

/// The panel as its own controller addresses it, which is portrait.
///
/// Not the same numbers as [`WIDTH`] and [`HEIGHT`] and not interchangeable with
/// them: mipidsi applies the size and offset in framebuffer space and the rotation
/// afterwards, so landscape figures here are rejected outright — 160 is wider than an
/// ST7735's 132-column framebuffer, and `init` answers `InvalidDisplaySize`.
const NATIVE_SIZE: (u16, u16) = (HEIGHT, WIDTH);

/// Where this 80x160 panel sits in that 132x162 framebuffer.
///
/// The vendor's `x_gap = 26`, `y_gap = 1`, verbatim and in that order, because these
/// are framebuffer coordinates too. Their driver swaps them when it builds a
/// landscape address window; mipidsi rotates for us, so the swap here would be one
/// rotation too many.
const NATIVE_OFFSET: (u16, u16) = (26, 1);

/// What the lines are drawn in.
///
/// The largest font that still fits all five lines on an 80-pixel-high screen: five
/// rows of fifteen pixels leave five to spare, and `FONT_10X20` would fit only four
/// lines. Seventeen columns is what it costs, and what the host's wording is cut to.
/// Everything below is derived from it, so trying another is this one line — the
/// geometry travels to the host in [`BridgeToHost::Ready`] and the lines come back
/// already laid out for it.
const FONT: MonoFont<'static> = FONT_9X15;

/// Height of one row, which is one glyph's.
const ROW_HEIGHT: u16 = FONT.character_size.height as u16;

/// Characters that fit across one line.
pub const COLS: u8 = (WIDTH / FONT.character_size.width as u16) as u8;

/// Rows that fit down the screen, never more than the wire carries.
pub const ROWS: u8 = {
    let fits = (HEIGHT / ROW_HEIGHT) as usize;
    (if fits < PANEL_ROWS { fits } else { PANEL_ROWS }) as u8
};

// The host is told this geometry and trusts it. A font whose rows do not fit, or whose
// characters do not, would have it drawing off the bottom or off the side.
const _: () = assert!(ROWS as u16 * ROW_HEIGHT <= HEIGHT);
const _: () = assert!(COLS as u16 * FONT.character_size.width as u16 <= WIDTH);

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

/// The shortest gap between two decisions about whether to redraw.
///
/// `MIN_REDRAW_MS` only moves when a row is actually drawn, so on a settled screen it
/// stops advancing and stops gating: every pass of a loop that turns over about a
/// thousand times a second would otherwise copy the host's lines and compare all five,
/// and in fallback mode would format the whole screen from scratch, to conclude each
/// time that nothing had changed. This bounds that to ten times a second. It is not
/// `MIN_REDRAW_MS` itself because that would delay a real change by up to half a
/// second, which is the thing `MIN_REDRAW_MS` is deliberately set below 1 Hz to avoid.
const MIN_LOOK_MS: u64 = 100;

/// How long the host may go quiet before the panel says so.
///
/// Measured against the host's `status_interval` of five seconds
/// (`crates/wartui-core/src/engine.rs`), not against the 1 Hz panel rate: a push only
/// goes out when a line's text or colour changed, so a capture with nothing moving
/// sends nothing but that five-second `GetStatus`. At five this would go false in the
/// gap before every one of them and a working capture would flash "no host" for ever.
/// Ten is two of those polls, which is the same margin and the same reasoning as
/// `stall::HOST_PRESENT_WINDOW_MS`; the cost of waiting is only that a screen is
/// briefly out of date rather than briefly wrong.
const HOST_GONE_MS: u64 = 10_000;

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
/// The screen itself, out of `main`'s frame for the reason `OUTBOX` is out of it:
/// it is a few hundred bytes of state that lives for the whole program, and `main`
/// already sits at `.clippy.toml`'s threshold on the C5.
static SCREEN: StaticCell<Screen> = StaticCell::new();

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
    /// When the decision above was last taken, drawn or not.
    last_look_ms: u64,
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
    /// # Errors
    /// A short reason if the bus or the panel refuses, which the caller reports and
    /// then carries on without a screen. Deliberately not a panic: the panic handler
    /// reboots, so a board whose panel will not start would boot-loop instead of
    /// bridging — and a bridge that cannot draw is still a bridge. The failure has to
    /// reach the host to be fixed, and it cannot do that from inside a reset.
    ///
    /// # Panics
    /// If called twice. There is one screen and one `StaticCell` behind it.
    pub fn new(pins: Pins) -> Result<&'static mut Self, &'static str> {
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
            .map_err(|_| "SPI2 refused the panel's configuration")?
            .with_sck(pins.sck)
            .with_mosi(pins.mosi);
        // The panel never reads, so MISO is left unclaimed even though the board wires
        // it: GPIO7 is `SD_DAT0` and belongs to the slot.

        let cs = Output::new(pins.cs, Level::High, OutputConfig::default());
        let dc = Output::new(pins.dc, Level::Low, OutputConfig::default());
        let reset = Output::new(pins.rst, Level::High, OutputConfig::default());

        let mut delay = Delay::new();
        let device =
            ExclusiveDevice::new(spi, cs, delay).map_err(|_| "the panel's CS pin refused")?;
        let wire = SpiInterface::new(device, dc, TRANSFER.take());

        let mut display = Builder::new(ST7735s, wire)
            .reset_pin(reset)
            .display_size(NATIVE_SIZE.0, NATIVE_SIZE.1)
            .display_offset(NATIVE_OFFSET.0, NATIVE_OFFSET.1)
            // `INVON` is in the init list, with `INVOFF` commented out beside it.
            .invert_colors(ColorInversion::Inverted)
            // Not a default to inherit. See the module doc.
            .color_order(ColorOrder::Bgr)
            // Reverses rows and swaps them with columns, so MADCTL comes out 0xA0 —
            // 0xA8 with the colour order — which is the vendor's rotation 3.
            .orientation(Orientation::new().rotate(Rotation::Deg270))
            .init(&mut delay)
            .map_err(|_| "the panel refused its init sequence")?;
        display.clear(BACKGROUND).map_err(|_| "the panel would not clear")?;

        Ok(SCREEN.init(Self {
            display,
            shown: [const { None }; PANEL_ROWS],
            fallback: false,
            last_ms: 0,
            last_look_ms: 0,
            _backlight: backlight,
            _sd_cs: sd_cs,
        }))
    }

    /// Bring the screen up to date, and say whether anything was drawn.
    ///
    /// The return value is folded into the loop's `worked`, so a panel with nothing to
    /// say does not keep an idle bridge awake.
    pub fn render(&mut self, bridge: &mut Bridge, mac: [u8; 6]) -> bool {
        let now_ms = bridge.now_ms();
        // Cheapest test first, then the one that bounds the work below on a screen
        // that has settled; `MIN_LOOK_MS` says why one gate is not enough.
        if now_ms.saturating_sub(self.last_ms) < MIN_REDRAW_MS {
            return false;
        }
        if now_ms.saturating_sub(self.last_look_ms) < MIN_LOOK_MS {
            return false;
        }
        self.last_look_ms = now_ms;

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
        // Over the rows this screen has, not the rows the wire can carry. They are the
        // same number only by coincidence of the font, and clearing the difference
        // costs a transfer per row for pixels that are past the bottom of the glass.
        for row in 0..ROWS as usize {
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
        let style = MonoTextStyle::new(&FONT, colour(line.level));
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
