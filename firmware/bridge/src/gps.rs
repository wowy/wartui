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
//! **The I2C reads block the loop**, about 38 µs a byte measured, so a full [`READ_CHUNK`]
//! costs about 2.5–4 ms (`docs/t-dongle-c5-gps-findings.md`). GGA and RMC at 5 Hz plus GSA
//! and GSV once a second is roughly 1.6 KB/s, about 6% of loop time. A read is capped at
//! one chunk so no single stall grows past that. [`Receiver::poll`] reads no more often
//! than every [`POLL_FLOOR_MS`], except while the receiver has more waiting, so a backlog
//! drains a chunk a pass rather than a chunk a floor. An idle loop waits up to its tick
//! between passes, which bounds how late a sentence reaches the host.
//!
//! **Reads are batched into frames.** Each `Gps` frame takes a whole slot of the
//! outbox's bulk ring, the slots node sightings queue in, so a frame per read (30 to 40 a
//! second at 5 Hz) would evict sightings whenever USB backs up. The bridge appends reads
//! to one [`GpsBytes`] and sends it when the receiver has drained, the end of an epoch's
//! burst, or when another full read would not fit. That is about one frame per epoch, plus
//! a few for the once-a-second GSA and GSV burst. Bytes are never held across a poll that
//! found the receiver drained.
//!
//! The read follows u-blox's DDC protocol: registers `0xFD`–`0xFE` hold the bytes waiting,
//! big-endian, and register `0xFF` is the stream.

use esp_hal::Blocking;
use esp_hal::i2c::master::{Config, I2c, SoftwareTimeout};
use esp_hal::peripherals::{GPIO11, GPIO12, I2C0};
use esp_hal::time::{Duration, Rate};
use wartui_proto::link::GpsBytes;

/// The receiver's I2C address, u-blox's default.
const ADDRESS: u8 = 0x42;
/// DDC register holding the bytes waiting, big-endian, with `0xFE` after it.
const REG_WAITING: u8 = 0xFD;
/// DDC register the stream is read from.
const REG_STREAM: u8 = 0xFF;

/// The most bytes one I2C read takes, which bounds how long one read blocks the loop.
pub const READ_CHUNK: usize = 64;

/// The shortest gap between polls while the receiver answers.
pub const POLL_FLOOR_MS: u64 = 25;

/// The gap between probes while nothing answers. A missing receiver then costs one
/// address NACK a second.
const ABSENT_PROBE_MS: u64 = 1000;

/// The longest one transaction may take. A full chunk is under 4 ms; this bounds a bus held
/// low by a receiver that is mid-reset or half-plugged, so it costs a read, not the loop.
const TRANSACTION_TIMEOUT_MS: u64 = 10;

/// The peripherals the receiver needs.
pub struct Pins {
    /// I2C0, which nothing else in this firmware uses.
    pub i2c: I2C0<'static>,
    /// Qwiic P3 pin 3, SDA.
    pub sda: GPIO11<'static>,
    /// Qwiic P3 pin 4, SCL.
    pub scl: GPIO12<'static>,
}

/// The receiver, and whether it answered last time.
pub struct Receiver {
    i2c: I2c<'static, Blocking>,
    /// Assumed until a probe fails, so the first failure is reported like any later one.
    present: bool,
    /// The earliest `now_ms` at which [`Receiver::poll`] touches the bus again.
    next_poll_ms: u64,
}

impl Receiver {
    /// Bring up the bus. Nothing is sent until the first [`Receiver::poll`].
    pub fn new(pins: Pins) -> Result<Self, &'static str> {
        let config = Config::default().with_frequency(Rate::from_khz(400)).with_software_timeout(
            SoftwareTimeout::Transaction(Duration::from_millis(TRANSACTION_TIMEOUT_MS)),
        );
        let i2c = I2c::new(pins.i2c, config)
            .map_err(|_| "the GPS port's I2C bus refused its configuration")?
            .with_sda(pins.sda)
            .with_scl(pins.scl);
        Ok(Self { i2c, present: true, next_poll_ms: 0 })
    }

    /// Whether the receiver answered the last probe.
    pub const fn present(&self) -> bool {
        self.present
    }

    /// Whether [`Receiver::poll`] would touch the bus at `now_ms`.
    pub const fn due(&self, now_ms: u64) -> bool {
        now_ms >= self.next_poll_ms
    }

    /// Append what the receiver has waiting to `buf`, up to one [`READ_CHUNK`] or the room
    /// left, if a poll is due. Says whether the receiver has more waiting: `false` means it
    /// has drained, did not answer, or the poll was not due.
    pub fn poll(&mut self, now_ms: u64, buf: &mut GpsBytes) -> bool {
        if !self.due(now_ms) {
            return false;
        }
        let mut waiting = [0u8; 2];
        if self.i2c.write_read(ADDRESS, &[REG_WAITING], &mut waiting).is_err() {
            self.absent(now_ms);
            return false;
        }
        self.present = true;
        self.next_poll_ms = now_ms + POLL_FLOOR_MS;
        // `0xFFFF` is what an idle bus reads as, not a count.
        let waiting = usize::from(u16::from_be_bytes(waiting));
        if waiting == 0 || waiting == usize::from(u16::MAX) {
            return false;
        }
        let start = buf.len();
        let len = waiting.min(READ_CHUNK).min(buf.capacity() - start);
        if len == 0 || buf.resize(start + len, 0).is_err() {
            return true;
        }
        if self.i2c.write_read(ADDRESS, &[REG_STREAM], &mut buf[start..]).is_err() {
            buf.truncate(start);
            self.absent(now_ms);
            return false;
        }
        let more = waiting > len;
        if more {
            self.next_poll_ms = now_ms;
        }
        more
    }

    /// Write bytes to the receiver verbatim, saying whether it took them.
    pub fn write(&mut self, bytes: &[u8]) -> bool {
        bytes.is_empty() || self.i2c.write(ADDRESS, bytes).is_ok()
    }

    const fn absent(&mut self, now_ms: u64) {
        self.present = false;
        self.next_poll_ms = now_ms + ABSENT_PROBE_MS;
    }
}
