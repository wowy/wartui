//! A u-blox receiver on the P4's I2C0, passed through to the host.
//!
//! SDA is GPIO7 and SCL is GPIO8, the pins the board pulls up with 2.2 kΩ. The board's
//! ES8311 codec shares the bus at its own address, and this firmware never addresses it.
//! The protocol, polling and batching are shared with the T-Dongle-C5
//! ([`wartui_firmware::gps`]); what is here is how this firmware runs them.
//!
//! **The reader is two more joined futures**, the shared loop and [`forward`], so it needs
//! no task and no interrupt executor. Everything on the P4 awaits, so the loop gets the
//! core back whenever a transfer needs starting. The channels take a [`NoopRawMutex`],
//! since both ends live in the one task, and their buffers are statics, so no 512-byte
//! batch is on a stack.
//!
//! **The write channel's sender sits in [`Shared`]**, where the USB reader hands it the
//! host's bytes; [`forward`] owns the batch receiver. Neither is borrowed across an
//! `.await`.
//!
//! **A batch read while no host holds DTR is dropped, not queued.** A fix is only worth
//! anything live: the host stamps bridge GPS bytes with their arrival time, and the host
//! queue holds minutes of them, so a backlog would land as minutes-old positions marked
//! current. The reader keeps draining the receiver either way. Other frames still queue
//! before a host attaches (#218).

use core::future::pending;

use embassy_futures::join::join;
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::zerocopy_channel::{Channel, Receiver, Sender};
use embassy_time::{Duration, Timer};
use esp_hal::peripherals::{GPIO7, GPIO8, I2C0};
use static_cell::{ConstStaticCell, StaticCell};
use wartui_firmware::gps::{self, ABSENT_PROBE_MS, BATCH_SLOTS, Bus, Watch};
use wartui_proto::link::{GpsBytes, GpsWriteBytes, LogLevel};

use crate::Shared;

/// What the bridge reports when the receiver stops answering.
const NO_GPS_RECEIVER: &str = "no GPS receiver answered on the I2C bus (GPIO7/GPIO8)";

static BATCH_BUF: ConstStaticCell<[GpsBytes; BATCH_SLOTS]> =
    ConstStaticCell::new([const { GpsBytes::new() }; BATCH_SLOTS]);
static BATCHES: StaticCell<Channel<'static, NoopRawMutex, GpsBytes>> = StaticCell::new();
static WRITE_BUF: ConstStaticCell<[GpsWriteBytes; 1]> =
    ConstStaticCell::new([GpsWriteBytes::new()]);
static WRITES: StaticCell<Channel<'static, NoopRawMutex, GpsWriteBytes>> = StaticCell::new();

/// The peripherals the receiver needs.
pub struct Pins {
    /// I2C0, which nothing else in this firmware uses.
    pub i2c: I2C0<'static>,
    /// SDA, shared with the ES8311.
    pub sda: GPIO7<'static>,
    /// SCL, shared with the ES8311.
    pub scl: GPIO8<'static>,
}

/// Bring up the bus and read the receiver forever. A bus that refuses its configuration
/// becomes the GPS fault, and this then waits forever.
pub async fn run(pins: Pins, shared: &Shared) -> ! {
    let Some(started) = start(pins, shared) else { pending().await };
    match join(
        gps::run(started.bus, started.batch_tx, started.write_rx),
        forward(shared, started.batch_rx),
    )
    .await {}
}

/// What [`start`] built: the bus and the shared loop's ends, and [`forward`]'s.
struct Started {
    bus: Bus,
    batch_tx: Sender<'static, NoopRawMutex, GpsBytes>,
    write_rx: Receiver<'static, NoopRawMutex, GpsWriteBytes>,
    batch_rx: Receiver<'static, NoopRawMutex, GpsBytes>,
}

/// Build the bus and the channels, and give [`Shared`] the write sender.
///
/// Out of line so the channels are built here rather than in the caller's poll frame.
#[inline(never)]
fn start(pins: Pins, shared: &Shared) -> Option<Started> {
    let i2c = match gps::bus(pins.i2c, pins.sda, pins.scl) {
        Ok(i2c) => i2c,
        Err(why) => {
            shared.gps_fault.set(Some(why));
            return None;
        }
    };
    let (batch_tx, batch_rx) = BATCHES.init(Channel::new(BATCH_BUF.take())).split();
    let (write_tx, write_rx) = WRITES.init(Channel::new(WRITE_BUF.take())).split();
    shared.gps_writes.replace(Some(write_tx));
    Some(Started { bus: Bus::new(i2c.into_async()), batch_tx, write_rx, batch_rx })
}

/// Forward each finished batch to a host holding DTR, drop it otherwise, and report what
/// changed about the receiver.
///
/// Wakes on a batch or every [`ABSENT_PROBE_MS`], so a receiver that stops answering is
/// reported within about a second even with no batch flowing. A change is said once: an
/// `Error` when it stops, which `announce` then repeats, and a `Log` when it comes back.
async fn forward(shared: &Shared, mut batches: Receiver<'static, NoopRawMutex, GpsBytes>) -> ! {
    let mut watch = Watch::new();
    loop {
        let probe = Timer::after(Duration::from_millis(ABSENT_PROBE_MS));
        if let Either::First(bytes) = select(batches.receive(), probe).await {
            if shared.dtr.get() {
                shared.push_gps(bytes);
            }
            batches.receive_done();
        }
        let changes = watch.changes();
        match changes.answered {
            Some(true) => {
                shared.gps_fault.set(None);
                shared.push_log(
                    LogLevel::Info,
                    "the GPS receiver answered on the I2C bus (GPIO7/GPIO8)",
                );
            }
            Some(false) => {
                shared.gps_fault.set(Some(NO_GPS_RECEIVER));
                shared.push_error(NO_GPS_RECEIVER);
            }
            None => {}
        }
        if changes.write_refused {
            shared.push_error("the GPS receiver refused a write");
        }
    }
}

/// Hand the host's bytes to the shared loop. Dropped when no bus came up, or when a write
/// is already pending.
pub fn write(shared: &Shared, bytes: &GpsWriteBytes) {
    if let Some(writes) = shared.gps_writes.borrow_mut().as_mut() {
        gps::write(writes, bytes);
    }
}
