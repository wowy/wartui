//! A GPS receiver on the T-Dongle-C5's Qwiic port.
//!
//! The receiver is a u-blox M10 (a SparkFun MAX-M10S) at I2C address `0x42`. The pins are
//! the vendor schematic's: the Qwiic connector is P3, whose pin 3 is GPIO11 and pin 4 is
//! GPIO12, and Qwiic orders its pins GND, 3V3, SDA, SCL. So SDA is GPIO11 and SCL is GPIO12.
//! The dongle has no pull-ups on them. The SparkFun board brings its own, and `esp-hal`
//! turns on the chip's weak ones as well.
//!
//! The receiver's protocol, polling and batching are shared with every bridge that has
//! one ([`wartui_firmware::gps`]). What is here is this board's: its pins, and how the
//! shared loop runs beside a `main` that blocks.
//!
//! **The receiver is read by its own task, over interrupt-driven I2C.** A full
//! [`READ_CHUNK`](gps::READ_CHUNK) takes 2.5–4 ms (`docs/t-dongle-c5-gps-findings.md`). Read blocking from
//! `main`, the CPU spins for all of it while the radio's ten-deep receive queue and the USB
//! outbox go unserved. With four nodes bursting, a 5 Hz receiver read that way lost 26–38
//! sighting batches a four-minute run against 0–17 without it, and the outbox dropped
//! frames.
//!
//! **The task runs on an interrupt executor, not on `main`'s.** The C5's I2C FIFO holds 32
//! bytes, so `esp-hal` splits a 64-byte read into three transfers, and the task must run to
//! start each one. On `main`'s executor it would run only when `main` gives the core back
//! between passes: a link burst or a panel repaint (36 ms) would leave a read paused
//! mid-transaction with the bus held, and the shared transaction timeout would have to
//! outlast both to mean anything. At [`Priority::Priority1`], the lowest interrupt level,
//! the task preempts `main` for the microseconds a transfer takes to start or finish, and
//! no interrupt handler waits on it.
//!
//! **`main` never waits on the task.** It forwards each finished batch on its next pass,
//! and a ready batch ends its idle wait. The channels take a [`CriticalSectionRawMutex`],
//! which the task's interrupt and `main`'s thread can both hold, and the task's slot is
//! filled in place, so no 512-byte batch is moved on the stack of whichever thread the
//! interrupt lands on.

use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::zerocopy_channel::{Channel, Receiver, Sender};
use esp_hal::Blocking;
use esp_hal::i2c::master::I2c;
use esp_hal::interrupt::Priority;
use esp_hal::peripherals::{FROM_CPU_INTR1, GPIO11, GPIO12, I2C0};
use esp_rtos::embassy::InterruptExecutor;
use static_cell::StaticCell;
use wartui_firmware::gps::{self, BATCH_SLOTS, Bus};
use wartui_proto::link::{GpsBytes, GpsWriteBytes};

/// The lock the channels take, which the task's interrupt and `main`'s thread can both hold.
type Lock = CriticalSectionRawMutex;

/// `main`'s end of the task.
pub type Link = gps::Link<'static, Lock>;

static EXECUTOR: StaticCell<InterruptExecutor<1>> = StaticCell::new();
static BATCH_BUF: StaticCell<[GpsBytes; BATCH_SLOTS]> = StaticCell::new();
static BATCHES: StaticCell<Channel<'static, Lock, GpsBytes>> = StaticCell::new();
static WRITE_BUF: StaticCell<[GpsWriteBytes; 1]> = StaticCell::new();
static WRITES: StaticCell<Channel<'static, Lock, GpsWriteBytes>> = StaticCell::new();

/// The peripherals the receiver needs.
pub struct Pins {
    /// I2C0, which nothing else in this firmware uses.
    pub i2c: I2C0<'static>,
    /// Qwiic P3 pin 3, SDA.
    pub sda: GPIO11<'static>,
    /// Qwiic P3 pin 4, SCL.
    pub scl: GPIO12<'static>,
}

/// Bring up the bus and start the task on its own interrupt executor. Nothing is sent
/// until the task's first poll.
///
/// Out of line so the channel buffers are built here rather than in `main`'s poll frame.
#[inline(never)]
pub fn start(pins: Pins, interrupt: FROM_CPU_INTR1<'static>) -> Result<Link, &'static str> {
    let i2c = gps::bus(pins.i2c, pins.sda, pins.scl)?;

    let batches = BATCHES
        .init(Channel::new(BATCH_BUF.init_with(|| [const { GpsBytes::new() }; BATCH_SLOTS])));
    let (batch_tx, batch_rx) = batches.split();
    let writes = WRITES.init(Channel::new(WRITE_BUF.init_with(|| [GpsWriteBytes::new()])));
    let (write_tx, write_rx) = writes.split();

    let token = launch(i2c, batch_tx, write_rx).map_err(|_| "the GPS task could not be started")?;
    EXECUTOR.init(InterruptExecutor::new(interrupt)).start(Priority::Priority1).spawn(token);
    Ok(Link::new(batch_rx, write_tx))
}

/// Start [`run`] from inside the interrupt executor.
///
/// An async driver is not `Send`, since its interrupt handler is bound to the core that
/// made it, and [`InterruptExecutor::start`] hands out only a spawner for `Send` tasks. So
/// the bus crosses over blocking, and this task, already on the executor, starts the one
/// that turns it async and keeps it.
#[embassy_executor::task]
async fn launch(
    i2c: I2c<'static, Blocking>,
    batches: Sender<'static, Lock, GpsBytes>,
    writes: Receiver<'static, Lock, GpsWriteBytes>,
) {
    let spawner = current_spawner().await;
    // `run` is spawned once, from the one `launch`, so its pool always has room.
    if let Ok(token) = run(Bus::new(i2c.into_async()), batches, writes) {
        spawner.spawn(token);
    }
}

/// The spawner of the executor the calling task runs on, which can start a task that is
/// not `Send`.
///
/// The firmware's one `unsafe` outside an IDF call. An interrupt executor gives out only a
/// `SendSpawner`, and the async I2C driver is not `Send`, so nothing safe reaches a spawner
/// that can start [`run`]; embassy names this call as the way for an interrupt executor.
#[allow(
    unsafe_code,
    reason = "an interrupt executor's spawner for non-Send tasks is reachable only through this call"
)]
async fn current_spawner() -> Spawner {
    // SAFETY: the call must be awaited in a `Context` an embassy executor created. This is
    // awaited only by `launch`, an embassy task, so the `Context` is its executor's.
    unsafe { Spawner::for_current_executor() }.await
}

/// The task: the shared loop, on this board's lock. A task cannot be generic, so this
/// names the one [`gps::run`] this firmware runs.
#[embassy_executor::task]
async fn run(
    bus: Bus,
    batches: Sender<'static, Lock, GpsBytes>,
    writes: Receiver<'static, Lock, GpsWriteBytes>,
) -> ! {
    gps::run(bus, batches, writes).await
}
