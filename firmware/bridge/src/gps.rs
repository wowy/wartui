//! A GPS receiver on the T-Dongle-C5's Qwiic port.
//!
//! The receiver is a u-blox M10 (a SparkFun MAX-M10S) at I2C address `0x42`. The pins are
//! the vendor schematic's: the Qwiic connector is P3, whose pin 3 is GPIO11 and pin 4 is
//! GPIO12, and Qwiic orders its pins GND, 3V3, SDA, SCL. So SDA is GPIO11 and SCL is GPIO12.
//! The dongle has no pull-ups on them. The SparkFun board brings its own, and `esp-hal`
//! turns on the chip's weak ones as well.
//!
//! **The bridge does not parse what the receiver says.** It reads the receiver's byte stream
//! and forwards it as [`BridgeToHost::Gps`](wartui_proto::link::BridgeToHost::Gps), and
//! writes the bytes a [`HostToBridge::GpsWrite`](wartui_proto::link::HostToBridge::GpsWrite)
//! carries. NMEA, UBX, the rate and the sentence set are the host's
//! (`crates/wartui-core/src/ubx.rs`), so a change to any of them costs a `cargo run`, not a
//! reflash.
//!
//! **The receiver is read by its own task, over interrupt-driven I2C.** A byte costs about
//! 38 µs on the bus, so a full [`READ_CHUNK`] takes 2.5–4 ms
//! (`docs/t-dongle-c5-gps-findings.md`). Read blocking from `main`, the CPU spins for all of
//! it while the radio's ten-deep receive queue and the USB outbox go unserved. With four
//! nodes bursting, a 5 Hz receiver read that way lost 26–38 sighting batches a four-minute
//! run against 0–17 without it, and the outbox dropped frames. Async I2C lets the peripheral
//! clock the bytes while the CPU serves the radio and USB.
//!
//! **The task runs on an interrupt executor, not on `main`'s.** The C5's I2C FIFO holds 32
//! bytes, so `esp-hal` splits a 64-byte read into three transfers, and the task must run to
//! start each one. On `main`'s executor it would run only when `main` gives the core back
//! between passes: a link burst or a panel repaint (36 ms) would leave a read paused
//! mid-transaction with the bus held, and [`TRANSACTION_TIMEOUT_MS`] would have to outlast
//! both to mean anything. At
//! [`Priority::Priority1`], the lowest interrupt level, the task preempts `main` for the
//! microseconds a transfer takes to start or finish, and no interrupt handler waits on it.
//!
//! **`main` never waits on the task.** They share two zero-copy channels and two flags:
//!
//! - finished batches go up [`BATCH_SLOTS`] deep; `main` forwards each on its next pass,
//!   and a ready batch ends its idle wait;
//! - a host write comes down one slot deep, and a write that finds it full is dropped,
//!   since the host repeats its configuration;
//! - whether the receiver answered, and whether a write was refused, are atomics `main`
//!   reads each pass, so the outbox and its `Error` and `Log` lines stay `main`'s.
//!
//! The task fills a slot in place and `main` copies out of it, so no 512-byte batch is
//! ever moved on the stack of whichever thread the interrupt lands on.
//!
//! **The task polls no more often than every [`POLL_FLOOR_MS`]**, and again straight away
//! while the receiver has more waiting, so a backlog drains. A receiver that does not
//! answer is probed every [`ABSENT_PROBE_MS`]. Every transaction is bounded by
//! [`TRANSACTION_TIMEOUT_MS`]; one that overruns is dropped, and `esp-hal` resets the
//! peripheral and clears the bus as it goes.
//!
//! **Reads are batched into frames.** Each `Gps` frame takes a whole slot of the
//! outbox's bulk ring, the slots node sightings queue in, so a frame per read (30 to 40 a
//! second at 5 Hz) would evict sightings whenever USB backs up. The task appends reads
//! to one [`GpsBytes`] and hands it up when the receiver has drained, the end of an epoch's
//! burst, or when another full read would not fit. That is about one frame per epoch, plus
//! a few for the once-a-second GSA and GSV burst. Bytes are never held across a poll that
//! found the receiver drained.
//!
//! The read follows u-blox's DDC protocol: registers `0xFD`–`0xFE` hold the bytes waiting,
//! big-endian, and register `0xFF` is the stream.

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::zerocopy_channel::{Channel, Receiver, Sender};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use esp_hal::i2c::master::{Config, I2c};
use esp_hal::interrupt::Priority;
use esp_hal::peripherals::{FROM_CPU_INTR1, GPIO11, GPIO12, I2C0};
use esp_hal::time::Rate;
use esp_hal::{Async, Blocking};
use esp_rtos::embassy::InterruptExecutor;
use portable_atomic::{AtomicBool, Ordering};
use static_cell::StaticCell;
use wartui_proto::link::{GpsBytes, GpsWriteBytes};

/// The receiver's I2C address, u-blox's default.
const ADDRESS: u8 = 0x42;
/// DDC register holding the bytes waiting, big-endian, with `0xFE` after it.
const REG_WAITING: u8 = 0xFD;
/// DDC register the stream is read from.
const REG_STREAM: u8 = 0xFF;

/// The most bytes one I2C read takes, which bounds how long one transaction holds the bus.
pub const READ_CHUNK: usize = 64;

/// The shortest gap between polls while the receiver answers.
const POLL_FLOOR_MS: u64 = 25;

/// The gap between probes while nothing answers. A missing receiver then costs one
/// address NACK a second.
const ABSENT_PROBE_MS: u64 = 1000;

/// The longest one transaction may take. A full chunk is under 4 ms; this bounds a bus held
/// low by a receiver that is mid-reset or half-plugged, so it costs a read, not the task.
const TRANSACTION_TIMEOUT_MS: u64 = 10;

/// Batches the task can have finished before `main` takes one: one waiting while the next
/// fills. A task that finds none free stops reading until `main` frees one, and the
/// receiver buffers meanwhile.
const BATCH_SLOTS: usize = 2;

/// The lock the channels take, which the task's interrupt and `main`'s thread can both hold.
type Lock = CriticalSectionRawMutex;

/// Whether the receiver answered its last poll. Assumed until a probe fails, so the first
/// failure is reported like any later one.
static PRESENT: AtomicBool = AtomicBool::new(true);

/// Set when the receiver refused a host write, and cleared by the pass that reports it.
static WRITE_REFUSED: AtomicBool = AtomicBool::new(false);

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

/// `main`'s end of the task.
pub struct Link {
    batches: Receiver<'static, Lock, GpsBytes>,
    writes: Sender<'static, Lock, GpsWriteBytes>,
}

impl Link {
    /// Whether the receiver answered its last poll.
    pub fn present(&self) -> bool {
        PRESENT.load(Ordering::Relaxed)
    }

    /// Whether the receiver refused a write since the last call.
    pub fn take_write_refused(&self) -> bool {
        WRITE_REFUSED.swap(false, Ordering::Relaxed)
    }

    /// Hand the task the host's bytes. Dropped when a write is already pending.
    pub fn write(&mut self, bytes: &GpsWriteBytes) {
        if let Some(slot) = self.writes.try_send() {
            slot.clone_from(bytes);
            self.writes.send_done();
        }
    }

    /// Finished batches, oldest first. Release each with `receive_done`.
    pub const fn batches(&mut self) -> &mut Receiver<'static, Lock, GpsBytes> {
        &mut self.batches
    }
}

/// Bring up the bus and start the task on its own interrupt executor. Nothing is sent
/// until the task's first poll.
///
/// Out of line so the channel buffers are built here rather than in `main`'s poll frame.
#[inline(never)]
pub fn start(pins: Pins, interrupt: FROM_CPU_INTR1<'static>) -> Result<Link, &'static str> {
    let config = Config::default().with_frequency(Rate::from_khz(400));
    let i2c = I2c::new(pins.i2c, config)
        .map_err(|_| "the GPS port's I2C bus refused its configuration")?
        .with_sda(pins.sda)
        .with_scl(pins.scl);

    let batches = BATCHES
        .init(Channel::new(BATCH_BUF.init_with(|| [const { GpsBytes::new() }; BATCH_SLOTS])));
    let (batch_tx, batch_rx) = batches.split();
    let writes = WRITES.init(Channel::new(WRITE_BUF.init_with(|| [GpsWriteBytes::new()])));
    let (write_tx, write_rx) = writes.split();

    let token = launch(i2c, batch_tx, write_rx).map_err(|_| "the GPS task could not be started")?;
    EXECUTOR.init(InterruptExecutor::new(interrupt)).start(Priority::Priority1).spawn(token);
    Ok(Link { batches: batch_rx, writes: write_tx })
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
    if let Ok(token) = run(Bus { i2c: i2c.into_async() }, batches, writes) {
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

/// What one poll found.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Polled {
    /// The receiver has nothing more waiting.
    Drained,
    /// The receiver has more waiting than was read.
    More,
    /// The receiver did not answer.
    Absent,
}

/// The task: poll the receiver, batch what it sends, and write what the host hands down.
#[embassy_executor::task]
async fn run(
    mut bus: Bus,
    mut batches: Sender<'static, Lock, GpsBytes>,
    mut writes: Receiver<'static, Lock, GpsWriteBytes>,
) -> ! {
    let mut next_poll = Instant::now();
    loop {
        let batch = batches.send().await;
        batch.clear();
        loop {
            match select(Timer::at(next_poll), writes.receive()).await {
                Either::First(()) => {
                    let started = Instant::now();
                    let polled = bus.poll(batch).await;
                    next_poll = match polled {
                        Polled::More => Instant::now(),
                        Polled::Drained => started + Duration::from_millis(POLL_FLOOR_MS),
                        Polled::Absent => started + Duration::from_millis(ABSENT_PROBE_MS),
                    };
                    let full = batch.capacity() - batch.len() < READ_CHUNK;
                    if (polled != Polled::More || full) && !batch.is_empty() {
                        break;
                    }
                }
                Either::Second(bytes) => {
                    // Skipped while the receiver is not answering: the host repeats its
                    // configuration, and `main`'s `gps_fault` already says why.
                    if PRESENT.load(Ordering::Relaxed) && !bus.write(bytes).await {
                        WRITE_REFUSED.store(true, Ordering::Relaxed);
                    }
                    writes.receive_done();
                }
            }
        }
        batches.send_done();
    }
}

/// The receiver's bus.
struct Bus {
    i2c: I2c<'static, Async>,
}

impl Bus {
    /// Append what the receiver has waiting to `buf`, up to one [`READ_CHUNK`] or the room
    /// left.
    async fn poll(&mut self, buf: &mut GpsBytes) -> Polled {
        let mut waiting = [0u8; 2];
        if !self.read(REG_WAITING, &mut waiting).await {
            PRESENT.store(false, Ordering::Relaxed);
            return Polled::Absent;
        }
        PRESENT.store(true, Ordering::Relaxed);
        // `0xFFFF` is what an idle bus reads as, not a count.
        let waiting = usize::from(u16::from_be_bytes(waiting));
        if waiting == 0 || waiting == usize::from(u16::MAX) {
            return Polled::Drained;
        }
        let start = buf.len();
        let len = waiting.min(READ_CHUNK).min(buf.capacity() - start);
        if len == 0 || buf.resize(start + len, 0).is_err() {
            return Polled::More;
        }
        if !self.read(REG_STREAM, &mut buf[start..]).await {
            buf.truncate(start);
            PRESENT.store(false, Ordering::Relaxed);
            return Polled::Absent;
        }
        if waiting > len { Polled::More } else { Polled::Drained }
    }

    /// Read register `reg` into `buf`, saying whether the receiver answered in time.
    async fn read(&mut self, reg: u8, buf: &mut [u8]) -> bool {
        let reg = [reg];
        let read = self.i2c.write_read_async(ADDRESS, &reg, buf);
        matches!(with_timeout(timeout(), read).await, Ok(Ok(())))
    }

    /// Write bytes to the receiver verbatim, saying whether it took them in time.
    async fn write(&mut self, bytes: &[u8]) -> bool {
        bytes.is_empty()
            || matches!(
                with_timeout(timeout(), self.i2c.write_async(ADDRESS, bytes)).await,
                Ok(Ok(()))
            )
    }
}

const fn timeout() -> Duration {
    Duration::from_millis(TRANSACTION_TIMEOUT_MS)
}
