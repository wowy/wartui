//! A u-blox GPS receiver on I2C: polled, batched, and passed through unread.
//!
//! One copy for every bridge with a receiver on its I2C bus. The T-Dongle-C5 reads one on
//! its Qwiic port, and the P4 bridge on GPIO7 and GPIO8. Each firmware owns its pins, its
//! channel buffers and how it runs [`run`], so this module holds only what the receiver and
//! the host need from either.
//!
//! **The bridge does not parse what the receiver says.** It reads the receiver's byte stream
//! and forwards it as [`BridgeToHost::Gps`](wartui_proto::link::BridgeToHost::Gps), and
//! writes the bytes a [`HostToBridge::GpsWrite`](wartui_proto::link::HostToBridge::GpsWrite)
//! carries. NMEA, UBX, the rate and the sentence set are the host's
//! (`crates/wartui-core/src/ubx.rs`), so a change to any of them costs a `cargo run`, not a
//! reflash.
//!
//! **The bus is driven async.** A byte costs about 38 µs on the bus, so a full
//! [`READ_CHUNK`] takes 2.5–4 ms (`docs/t-dongle-c5-gps-findings.md`). Async I2C lets the
//! peripheral clock the bytes while the CPU serves the radio and USB.
//!
//! **[`run`] talks to the rest of the firmware through two zero-copy channels and two
//! flags:**
//!
//! - finished batches go up, [`BATCH_SLOTS`] deep;
//! - a host write comes down one slot deep, and a write that finds it full is dropped,
//!   since the host repeats its configuration ([`write`]);
//! - whether the receiver answered, and whether a write was refused, are atomics a
//!   [`Watch`] reads, and it hands the firmware only what changed, so the firmware's
//!   `Error` and `Log` lines stay its own.
//!
//! [`run`] fills a slot in place and the firmware copies out of it, so no 512-byte batch
//! is ever moved on a stack. The channels are generic over their lock, so a firmware that
//! reads them from an interrupt executor and one that joins everything in one task each
//! pick theirs.
//!
//! **[`run`] polls no more often than every [`POLL_FLOOR_MS`]**, and again straight away
//! while the receiver has more waiting, so a backlog drains. A receiver that does not
//! answer is probed every [`ABSENT_PROBE_MS`]. Every transaction is bounded by
//! [`TRANSACTION_TIMEOUT_MS`]; one that overruns is dropped, and `esp-hal` resets the
//! peripheral and clears the bus as it goes.
//!
//! **Reads are batched into frames.** Each `Gps` frame takes a whole slot of the
//! outbox's bulk ring, the slots node sightings queue in, so a frame per read (30 to 40 a
//! second at 5 Hz) would evict sightings whenever USB backs up. [`run`] appends reads
//! to one [`GpsBytes`] and hands it up when the receiver has drained, the end of an epoch's
//! burst, or when another full read would not fit. That is about one frame per epoch, plus
//! a few for the once-a-second GSA and GSV burst. Bytes are never held across a poll that
//! found the receiver drained.
//!
//! The read follows u-blox's DDC protocol: registers `0xFD`–`0xFE` hold the bytes waiting,
//! big-endian, and register `0xFF` is the stream.

use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::zerocopy_channel::{Receiver, Sender};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use esp_hal::gpio::interconnect::{PeripheralInput, PeripheralOutput};
use esp_hal::i2c::master::{Config, I2c, Instance};
use esp_hal::time::Rate;
use esp_hal::{Async, Blocking};
use portable_atomic::{AtomicBool, Ordering};
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
pub const ABSENT_PROBE_MS: u64 = 1000;

/// The longest one transaction may take. A full chunk is under 4 ms; this bounds a bus held
/// low by a receiver that is mid-reset or half-plugged, so it costs a read, not the loop.
const TRANSACTION_TIMEOUT_MS: u64 = 10;

/// Batches [`run`] can have finished before the firmware takes one: one waiting while the
/// next fills. A [`run`] that finds none free stops reading until the firmware frees one,
/// and the receiver buffers meanwhile. Size the batch channel's buffer to this.
pub const BATCH_SLOTS: usize = 2;

/// Whether the receiver answered its last poll. Assumed until a probe fails, so the first
/// failure is reported like any later one.
static PRESENT: AtomicBool = AtomicBool::new(true);

/// Set when the receiver refused a host write, and cleared by the read that reports it.
static WRITE_REFUSED: AtomicBool = AtomicBool::new(false);

/// What changed about the receiver since a [`Watch`] last looked.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Changes {
    /// `Some(false)` when the receiver stopped answering, `Some(true)` when it answered
    /// again, and `None` when neither.
    pub answered: Option<bool>,
    /// Whether the receiver refused a host write. Independent of `answered`: one look can
    /// see both.
    pub write_refused: bool,
}

/// The firmware's view of what [`run`] learned about the receiver, so a change is said
/// once rather than at every probe.
pub struct Watch {
    /// Whether the receiver answered when [`Watch::changes`] last looked.
    answered: bool,
}

impl Watch {
    /// Starts believing the receiver answers, as [`run`] does, so the first failed probe
    /// is a change like any later one.
    pub const fn new() -> Self {
        Self { answered: true }
    }

    /// What changed since the last call.
    pub fn changes(&mut self) -> Changes {
        let present = PRESENT.load(Ordering::Relaxed);
        let write_refused = WRITE_REFUSED.swap(false, Ordering::Relaxed);
        let answered = (present != self.answered).then_some(present);
        self.answered = present;
        Changes { answered, write_refused }
    }
}

impl Default for Watch {
    fn default() -> Self {
        Self::new()
    }
}

/// Hand [`run`] the host's bytes through `writes`, the write channel's sending end.
/// Dropped when a write is already pending, since the host repeats its configuration.
pub fn write<M: RawMutex>(writes: &mut Sender<'_, M, GpsWriteBytes>, bytes: &GpsWriteBytes) {
    if let Some(slot) = writes.try_send() {
        slot.clone_from(bytes);
        writes.send_done();
    }
}

/// Bring up the receiver's bus at 400 kHz, blocking. The caller turns it async where the
/// driver's interrupt handler is to be bound, then wraps it in a [`Bus`].
///
/// # Errors
///
/// When the peripheral refuses the configuration, a message for the host.
pub fn bus<'d>(
    i2c: impl Instance + 'd,
    sda: impl PeripheralInput<'d> + PeripheralOutput<'d>,
    scl: impl PeripheralInput<'d> + PeripheralOutput<'d>,
) -> Result<I2c<'d, Blocking>, &'static str> {
    let config = Config::default().with_frequency(Rate::from_khz(400));
    Ok(I2c::new(i2c, config)
        .map_err(|_| "the GPS port's I2C bus refused its configuration")?
        .with_sda(sda)
        .with_scl(scl))
}

/// The firmware's end of [`run`].
pub struct Link<'a, M: RawMutex> {
    batches: Receiver<'a, M, GpsBytes>,
    writes: Sender<'a, M, GpsWriteBytes>,
}

impl<'a, M: RawMutex> Link<'a, M> {
    /// The ends of the two channels whose other ends [`run`] holds.
    pub const fn new(
        batches: Receiver<'a, M, GpsBytes>,
        writes: Sender<'a, M, GpsWriteBytes>,
    ) -> Self {
        Self { batches, writes }
    }

    /// Hand [`run`] the host's bytes. Dropped when a write is already pending.
    pub fn write(&mut self, bytes: &GpsWriteBytes) {
        write(&mut self.writes, bytes);
    }

    /// Finished batches, oldest first. Release each with `receive_done`.
    pub const fn batches(&mut self) -> &mut Receiver<'a, M, GpsBytes> {
        &mut self.batches
    }
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

/// Poll the receiver, batch what it sends, and write what the host hands down. Never
/// returns.
///
/// A plain future rather than a task, because an embassy task cannot be generic: a
/// firmware spawns a task that awaits it, or joins it with its own loop.
pub async fn run<M: RawMutex>(
    mut bus: Bus,
    mut batches: Sender<'_, M, GpsBytes>,
    mut writes: Receiver<'_, M, GpsWriteBytes>,
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
                    // configuration, and the firmware's GPS fault already says why.
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

/// The receiver's bus, async.
pub struct Bus {
    i2c: I2c<'static, Async>,
}

impl Bus {
    /// Wrap a bus [`bus`] brought up and the caller turned async.
    pub const fn new(i2c: I2c<'static, Async>) -> Self {
        Self { i2c }
    }

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
