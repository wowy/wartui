//! The carrier's lid fan, a Noctua NF-A6x15 5V PWM: its PWM on GPIO51, its tach on GPIO52, and
//! the duty the P4 keeps on flash. `wartui_proto::fan` has the wave, the tach arithmetic and the
//! record's layout.
//!
//! **Only on a carrier.** The bench has no fan header, so there [`run`] never configures GPIO51
//! or GPIO52, the stored duty is never read or written, a `SetFan` is ignored and `Status.fan` is
//! `None`.
//!
//! **The P4 is the duty's system of record.** It boots into the duty stored in the first sector
//! of its `nvs` partition, or 100 % when none is stored or the partition does not read, before
//! the futures start, and holds it with no host present. A `SetFan` ([`set`]) applies at once and
//! is written only when it differs from the stored duty, so a repeat costs no flash. Each write
//! parks core 1, as every P4 flash write does (`wartui_firmware::ota`), and one change in 1024
//! erases the sector first. A write that fails leaves the duty applied until reboot and says so
//! to the host.
//!
//! **GPIO51 is inverted**: high holds the fan's PWM input low. The RMT drives it, since this
//! esp-hal has no LEDC driver for the P4. Its counter runs at [`RMT_HZ`], so one 40 µs period at
//! [`PWM_HZ`] is [`PERIOD_TICKS`]. A wave is one pulse code, high then low, looped forever; 0 % is
//! one code high in both halves. 100 % stops the channel, which leaves the pin at its idle low,
//! as the P4 in reset leaves the fan at full speed. The channel restarts only when the wave
//! changes.
//!
//! **GPIO52 counts falling edges**, which the board pulls up, so it needs no pull of its own.
//! [`TachFilter`] drops ringing, and each [`TACH_WINDOW_MS`] gives one reading. No fan plugged in
//! reads 0 rpm.

use core::cell::{Cell, RefCell};
use core::fmt::{self, Write};
use core::future::pending;

use embassy_futures::join::join;
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::Blocking;
use esp_hal::gpio::{Input, InputConfig, Level};
use esp_hal::peripherals::{GPIO51, GPIO52, RMT};
use esp_hal::rmt::{
    Channel, ContinuousTxTransaction, LoopMode, PulseCode, Rmt, Tx, TxChannelConfig,
    TxChannelCreator,
};
use esp_hal::time::Rate;
use static_cell::{ConstStaticCell, StaticCell};
use wartui_firmware::ota::Fault;
use wartui_proto::fan::store::{self, Next};
use wartui_proto::fan::{FanDuty, FanStats, PWM_HZ, Pwm, TACH_WINDOW_MS, TachFilter, pwm, rpm};
use wartui_proto::link::LogStr;
use wartui_proto::ota::SECTOR;

use crate::Shared;
use crate::update::Updates;

/// The RMT counter's clock: 80 MHz divided by 8.
const RMT_HZ: u32 = 10_000_000;

/// Ticks in one PWM period: 400 for 40 µs.
#[allow(clippy::cast_possible_truncation)]
const PERIOD_TICKS: u16 = (RMT_HZ / PWM_HZ) as u16;

/// The sector's image, read before each write. Static, to keep 4 KB off the stack.
static SECTOR_IMAGE: ConstStaticCell<[u8; SECTOR]> = ConstStaticCell::new([0; SECTOR]);

/// Static, to keep it out of `main`'s future.
static FAN: StaticCell<Fan> = StaticCell::new();

/// The pins and the peripheral the fan takes, untouched until [`run`] finds a carrier.
pub struct Pins {
    pub rmt: RMT<'static>,
    pub pwm: GPIO51<'static>,
    pub tach: GPIO52<'static>,
}

/// The fan as the host reader and [`run`] see it.
pub struct Fan {
    /// The duty in force.
    duty: Cell<FanDuty>,
    /// The last whole window's speed, `None` before the first.
    rpm: Cell<Option<u16>>,
    /// Raised when `duty` changes, so [`run`] applies it.
    changed: Signal<NoopRawMutex, ()>,
    sector: RefCell<&'static mut [u8; SECTOR]>,
}

impl Fan {
    /// The fan at the stored duty, or 100 % when none reads. Called once, on a carrier, before
    /// the futures start.
    ///
    /// Out of line, as `update::boot` is.
    ///
    /// # Panics
    /// If called twice: the fan and its sector image are statics taken once.
    #[inline(never)]
    pub fn boot(updates: &mut Updates) -> &'static Self {
        let sector = SECTOR_IMAGE.take();
        let stored = updates
            .settings()
            .and_then(|mut settings| settings.read(sector).ok())
            .and_then(|()| store::read(sector));
        FAN.init_with(|| Self {
            duty: Cell::new(stored.unwrap_or(FanDuty::FULL)),
            rpm: Cell::new(None),
            changed: Signal::new(),
            sector: RefCell::new(sector),
        })
    }

    /// What `Status` reports.
    pub fn stats(&self) -> FanStats {
        FanStats { duty: self.duty.get(), rpm: self.rpm.get() }
    }
}

/// A `SetFan`: run at `duty` now, and store it when it differs from the stored duty. Ignored with
/// no carrier, and a no-op at the duty in force.
#[inline(never)]
pub fn set(shared: &Shared, duty: FanDuty) {
    let Some(fan) = shared.fan else { return };
    if fan.duty.get() == duty {
        return;
    }
    fan.duty.set(duty);
    fan.changed.signal(());
    if let Err(why) = save(shared, fan, duty) {
        let mut message = LogStr::new();
        let _ = write!(message, "fan duty {duty} not saved, so it holds until reboot: {why}");
        shared.push_error(&message);
    }
}

/// Why a duty was not saved.
enum Unsaved {
    /// The table has no `nvs` partition.
    NoPartition,
    /// Reading, erasing or writing the sector.
    Flash(Fault),
}

impl fmt::Display for Unsaved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoPartition => f.write_str("no nvs partition"),
            Self::Flash(err) => write!(f, "{err}"),
        }
    }
}

impl From<Fault> for Unsaved {
    fn from(err: Fault) -> Self {
        Self::Flash(err)
    }
}

/// Write `duty` at the sector's next erased word, erasing it first when it is full, unless the
/// sector already holds `duty`.
fn save(shared: &Shared, fan: &Fan, duty: FanDuty) -> Result<(), Unsaved> {
    let mut updates = shared.updates.borrow_mut();
    let mut settings = updates.settings().ok_or(Unsaved::NoPartition)?;
    let mut image = fan.sector.borrow_mut();
    settings.read(&mut image)?;
    if store::read(&image) == Some(duty) {
        return Ok(());
    }
    let at = match store::next(&image) {
        Next::At(at) => at,
        Next::Erase => {
            settings.erase()?;
            0
        }
    };
    // `at` is inside one sector.
    #[allow(clippy::cast_possible_truncation)]
    settings.write(at as u32, &store::record(duty))?;
    Ok(())
}

/// Drive the fan and read its tach forever, on a carrier; on the bench, wait forever and touch
/// neither pin.
pub async fn run(pins: Pins, shared: &Shared) -> ! {
    let Some(fan) = shared.fan else { pending().await };
    let Pins { rmt, pwm, tach } = pins;
    // The pin stays undriven, so the fan at full speed, if the RMT does not come up.
    let config = TxChannelConfig::default()
        .with_clk_divider(1)
        .with_idle_output(true)
        .with_idle_output_level(Level::Low);
    let channel = Rmt::new(rmt, Rate::from_hz(RMT_HZ))
        .ok()
        .and_then(|rmt| rmt.channel0.configure_tx(&config).ok())
        .map(|channel| channel.with_pin(pwm));
    let tach = Input::new(tach, InputConfig::default());
    join(drive(channel, fan), count(tach, fan)).await.0
}

/// The RMT channel, stopped or looping one wave.
enum Output {
    Stopped(Channel<'static, Blocking, Tx>),
    Looping(ContinuousTxTransaction<'static>),
}

/// Apply the duty in force, and again whenever it changes.
async fn drive(channel: Option<Channel<'static, Blocking, Tx>>, fan: &Fan) -> ! {
    let Some(channel) = channel else { pending().await };
    let mut output = Output::Stopped(channel);
    let mut applied = None;
    loop {
        let wave = pwm(fan.duty.get(), PERIOD_TICKS);
        if applied != Some(wave) {
            output = apply(output, wave);
            applied = Some(wave);
        }
        fan.changed.wait().await;
    }
}

/// Stop the channel, then loop `wave`'s code on it, if it has one.
fn apply(output: Output, wave: Pwm) -> Output {
    let channel = match output {
        Output::Stopped(channel) => channel,
        Output::Looping(looping) => match looping.stop() {
            Ok(channel) | Err((_, channel)) => channel,
        },
    };
    let code = match wave {
        Pwm::Full => return Output::Stopped(channel),
        Pwm::GpioHigh => {
            PulseCode::new(Level::High, PERIOD_TICKS / 2, Level::High, PERIOD_TICKS / 2)
        }
        Pwm::Wave { gpio_high, gpio_low } => {
            PulseCode::new(Level::High, gpio_high, Level::Low, gpio_low)
        }
    };
    // A zero length ends the code list; held high, as each period starts.
    let end = PulseCode::new(Level::High, 0, Level::High, 0);
    match channel.transmit_continuously(&[code, end], LoopMode::Infinite) {
        Ok(looping) => Output::Looping(looping),
        Err((_, channel)) => Output::Stopped(channel),
    }
}

/// Count tach edges over each window, and report the speed once it ends.
async fn count(mut tach: Input<'static>, fan: &Fan) -> ! {
    let window = Duration::from_millis(u64::from(TACH_WINDOW_MS));
    let mut filter = TachFilter::new();
    loop {
        let end = Instant::now() + window;
        let mut edges = 0u32;
        while let Either::First(()) = select(tach.wait_for_falling_edge(), Timer::at(end)).await {
            if filter.accept(Instant::now().as_micros()) {
                edges = edges.saturating_add(1);
            }
        }
        fan.rpm.set(Some(rpm(edges, TACH_WINDOW_MS)));
    }
}
