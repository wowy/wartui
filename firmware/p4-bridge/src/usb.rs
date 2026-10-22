//! The host link: wartui's link protocol over CDC-ACM on the P4's high-speed USB.
//!
//! The device enumerates only once [`Pair::usb_serial_mac`] names the address for its
//! serial number, the C6's when it answers in time. The host finds a bridge by that
//! serial, so enumerating early with another would name the wrong board.
//!
//! **Transmit opens on DTR.** The single-chip bridge writes nothing until a host has
//! spoken ([`ResetCause::speaks_first`]), because a write into USB-Serial-JTAG before any
//! host opens the port wedges the endpoint. CDC-ACM over the OTG controller does not
//! wedge, and the kernel raises DTR when a host opens the port, so DTR is the signal that
//! someone is reading. The writer waits on the CDC class's control-change signal for it.
//! A freshly enumerated tty raises DTR before the host sets raw mode, so it echoes what the
//! P4 sends in that window back to it. The host leads each connection with a lone `0x00`,
//! which ends that echo before its first frame.
//!
//! **Nothing queues without DTR, and a rising edge empties the queue.** A host never reads
//! a backlog from before it attached; `Shared::push_host` has why.
//!
//! [`Pair::usb_serial_mac`]: wartui_proto::split::Pair::usb_serial_mac
//! [`ResetCause::speaks_first`]: wartui_proto::reset::ResetCause::speaks_first

use core::fmt::Write;

use embassy_futures::join::join4;
use embassy_futures::select::select;
use embassy_futures::yield_now;
use embassy_time::{Duration, with_timeout};
use embassy_usb::Builder;
use embassy_usb::class::cdc_acm::{CdcAcmClass, ControlChanged, Receiver, Sender, State};
use embassy_usb::driver::Driver as UsbDriver;
use esp_hal::peripherals::USB_HS;
use esp_hal::usb::otg::Usb;
use esp_hal::usb::otg::embassy_usb_device::{Config as OtgConfig, Driver};
use static_cell::{ConstStaticCell, StaticCell};
use wartui_firmware::phase::mark;
use wartui_firmware::reboot;
use wartui_proto::heapless::String;
use wartui_proto::host_queue::HS_PACKET;
use wartui_proto::link::{
    BridgeToHost, FrameAccumulator, HostToBridge, LoopPhase, MAX_HOST_FRAME, SendStatus,
    decode_frame,
};
use wartui_proto::mac::{self, Mac};
use wartui_proto::spi::Down;
use wartui_proto::split::Route;

use crate::Shared;

/// How long a `GetStatus` waits for the C6's counters before answering without fresh ones.
const STATUS_WAIT: Duration = Duration::from_millis(50);

/// The serial number: a MAC as `AA:BB:CC:DD:EE:FF`, the form `mac::parse` reads.
const SERIAL_LEN: usize = 17;

/// OUT packets land here: the CDC data OUT endpoint's 512 plus EP0's 64, rounded up.
static EP_OUT_BUFFER: ConstStaticCell<[u8; 1024]> = ConstStaticCell::new([0; 1024]);
static CONFIG_DESCRIPTOR: ConstStaticCell<[u8; 256]> = ConstStaticCell::new([0; 256]);
static BOS_DESCRIPTOR: ConstStaticCell<[u8; 256]> = ConstStaticCell::new([0; 256]);
static CONTROL_BUF: ConstStaticCell<[u8; 64]> = ConstStaticCell::new([0; 64]);
static CDC_STATE: StaticCell<State<'static>> = StaticCell::new();
/// `embassy-usb` takes its strings as `&'static str`.
static SERIAL: StaticCell<String<SERIAL_LEN>> = StaticCell::new();

/// Wait until the serial number is known, then enumerate and serve the host forever.
pub async fn run(usb_hs: USB_HS<'static>, shared: &Shared) -> ! {
    let mac = loop {
        if let Some(mac) = shared.pair.borrow().usb_serial_mac() {
            break mac;
        }
        shared.paired.wait().await;
    };
    let driver = Driver::new(
        Usb::new_hs(usb_hs),
        EP_OUT_BUFFER.take(),
        // VBUS is not wired for sensing; the driver then forces VBUS-valid.
        OtgConfig::default(),
    );
    let mut config = embassy_usb::Config::new(0x1209, 0x0001);
    config.manufacturer = Some("wartui");
    config.product = Some("wartui p4 bridge");
    config.serial_number = Some(serial(mac));
    config.self_powered = true;
    config.max_power = 0;
    let mut builder = Builder::new(
        driver,
        config,
        CONFIG_DESCRIPTOR.take(),
        BOS_DESCRIPTOR.take(),
        &mut [],
        CONTROL_BUF.take(),
    );
    let class = CdcAcmClass::new(&mut builder, CDC_STATE.init(State::new()), HS_PACKET as u16);
    let mut device = builder.build();
    let (sender, receiver, control) = class.split_with_control();
    join4(
        device.run(),
        read_host(receiver, shared),
        write_host(sender, &control, shared),
        answer_status(shared),
    )
    .await
    .0
}

fn serial(mac: Mac) -> &'static str {
    let text = SERIAL.init(String::new());
    // 17 characters always fit.
    let _ = write!(text, "{}", mac::full(&mac));
    let text: &'static String<SERIAL_LEN> = text;
    text.as_str()
}

/// Read host frames forever and act on each.
async fn read_host<'d, D: UsbDriver<'d>>(mut rx: Receiver<'d, D>, shared: &Shared) -> ! {
    let mut packet = [0u8; HS_PACKET];
    let mut frames = FrameAccumulator::<MAX_HOST_FRAME>::new();
    loop {
        rx.wait_connection().await;
        while let Ok(n) = rx.read_packet(&mut packet).await {
            mark(LoopPhase::DrainLink);
            for &byte in &packet[..n] {
                if let Some(frame) = frames.push(byte) {
                    take_frame(frame, shared);
                }
            }
        }
        // Disconnected: a frame part-way in is never finished.
        frames.reset();
    }
}

/// Decode one host frame and send it where the pair says.
///
/// Out of line: a decoded command is 1.5 KB, and a refusal carries a frame. The command is
/// decoded straight into the `Down` that relays it, so it is never copied.
#[inline(never)]
fn take_frame(frame: &mut [u8], shared: &Shared) {
    // Matched in place rather than moved out of the `Option`, which would copy it.
    let decoded = decode(frame);
    let Some(down @ Down::Command(cmd)) = &decoded else { return };
    let route = shared.pair.borrow().route(cmd);
    match route {
        Route::Local => local(cmd, shared),
        Route::Forward => forward(down, shared),
        Route::Refuse(Some(answer)) => shared.push_host(&answer),
        Route::Refuse(None) => {}
    }
}

#[inline(never)]
fn decode(frame: &mut [u8]) -> Option<Down> {
    decode_frame::<HostToBridge>(frame).ok().map(Down::Command)
}

/// Carry out a command the P4 answers itself.
fn local(cmd: &HostToBridge, shared: &Shared) {
    mark(LoopPhase::Command);
    match cmd {
        HostToBridge::Identify => shared.announce(),
        HostToBridge::GetStatus => shared.status_asked.signal(()),
        HostToBridge::Reset => reboot(),
        #[cfg(feature = "gps")]
        HostToBridge::GpsWrite { bytes } => crate::gps::write(shared, bytes),
        // No GPS port in this build.
        #[cfg(not(feature = "gps"))]
        HostToBridge::GpsWrite { .. } => {}
        // No panel.
        HostToBridge::ShowPanel { .. } => {}
        // `Pair::route` sends nothing else here.
        _ => {}
    }
}

/// Queue a command for the C6. A send that finds the queue full is answered `Rejected`,
/// since nothing else would ever answer it.
fn forward(down: &Down, shared: &Shared) {
    if !shared.push_down(down)
        && let Down::Command(HostToBridge::SendEspNow { id, .. }) = *down
    {
        shared.push_host(&BridgeToHost::SendResult { id, status: SendStatus::Rejected, tx_us: 0 });
    }
}

/// Write queued frames to the host while it holds DTR.
async fn write_host<'d, D: UsbDriver<'d>>(
    mut tx: Sender<'d, D>,
    control: &ControlChanged<'d>,
    shared: &Shared,
) -> ! {
    let mut packet = [0u8; HS_PACKET];
    loop {
        tx.wait_connection().await;
        while !control.dtr() {
            control.control_changed().await;
        }
        // A new host: whatever is queued was for one that has gone. Cleared before the first
        // packet, and before the reader can see this host's `Identify`, so its `Ready` is not
        // lost with the rest.
        shared.host.borrow_mut().clear();
        shared.dtr.set(true);
        while control.dtr() {
            let n = shared.host.borrow_mut().next_packet(&mut packet);
            if n == 0 {
                // Wake on the next queued frame, or on DTR falling.
                select(shared.pushed.wait(), control.control_changed()).await;
                continue;
            }
            mark(LoopPhase::Pump);
            if tx.write_packet(&packet[..n]).await.is_err() {
                break;
            }
            // So the SPI link is polled at least once per packet.
            yield_now().await;
        }
        shared.dtr.set(false);
    }
}

/// Answer each `GetStatus` with the C6's counters as fresh as [`STATUS_WAIT`] allows.
async fn answer_status(shared: &Shared) -> ! {
    loop {
        shared.status_asked.wait().await;
        shared.radio_status.reset();
        let up = shared.pair.borrow().bridges();
        if up && shared.push_down(&Down::GetRadioStatus) {
            let _ = with_timeout(STATUS_WAIT, shared.radio_status.wait()).await;
        }
        shared.push_status();
    }
}
