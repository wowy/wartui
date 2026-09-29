//! What the two extended-advertising bench examples share: bringing the chip up
//! with the radio to Bluetooth alone, printing, the reset funnel, the positive
//! control's addresses and a raw HCI command that reports how it was answered.
//!
//! Neither example is the product. `src/ble.rs` is the node's conversation with
//! the controller and stays as it is until the bench says whether extended
//! scanning earns a place there; the pieces copied from it here carry its
//! reasoning by reference.

use esp_hal::clock::CpuClock;
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::peripherals::BT;
use esp_hal::time::Duration;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::ble::Config;
use esp_radio::ble::controller::BleConnector;
use esp_rtos::CurrentThreadHandle;
use wartui_proto::hci::{PACKET_MAX, command_complete};

#[cfg(not(any(feature = "esp32c5", feature = "esp32c6")))]
compile_error!("select a chip: --features esp32c5 or --features esp32c6");
#[cfg(all(feature = "esp32c5", feature = "esp32c6"))]
compile_error!("select exactly one chip: esp32c5 and esp32c6 are mutually exclusive");
#[cfg(all(feature = "xiao-external-antenna", not(feature = "esp32c6")))]
compile_error!("xiao-external-antenna drives a XIAO ESP32-C6's RF switch; build it with esp32c6");

/// `note!` from `src/main.rs`: a line on the USB endpoint when the build has one.
macro_rules! say {
    ($($arg:tt)*) => {{
        #[cfg(feature = "log")]
        esp_println::println!($($arg)*);
        #[cfg(not(feature = "log"))]
        {
            let _ = format_args!($($arg)*);
        }
    }};
}

/// The positive control's extended-only set on the 1M primary PHY.
///
/// Static random addresses: the two top bits of the first octet set, which is
/// what `HCI_LE_Set_Advertising_Set_Random_Address` accepts for a set that never
/// rotates. Chosen to be easy to grep for, and shared so the scanner and the
/// beacon cannot disagree.
pub const BEACON_1M: [u8; 6] = [0xC0, 0xDE, 0xBE, 0xAC, 0x00, 0x01];
/// The control's set on the Coded primary PHY.
pub const BEACON_CODED: [u8; 6] = [0xC0, 0xDE, 0xBE, 0xAC, 0x00, 0x02];

/// Granularity of the polling loops, as in `src/ble.rs`.
pub const POLL_MS: u64 = 2;

/// Reset the chip through the funnel `src/main.rs` documents.
fn reboot() -> ! {
    #[cfg(feature = "esp32c5")]
    esp_hal::peripherals::PCR::regs()
        .reset_event_bypass()
        .modify(|_, w| w.reset_event_bypass().clear_bit());

    esp_hal::system::software_reset()
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    say!("panic: {}", info);
    reboot()
}

/// Bring the chip up the way `src/main.rs` does, minus Wi-Fi and ESP-NOW, and
/// hand back the Bluetooth peripheral.
///
/// Without Wi-Fi the radio is Bluetooth's alone, which is the situation of the
/// node holding the scan: it sniffs nothing.
pub fn boot() -> BT<'static> {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    say!("reset reason: {:?}", esp_hal::system::reset_reason());

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let software_interrupt = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, software_interrupt.software_interrupt0);

    // The XIAO's RF switch, as `src/main.rs` sets it and for its reasons: GPIO3
    // low powers the switch, GPIO14 high 100 ms later selects the U.FL connector,
    // before the radio starts. `boot` returns and the drivers go out of scope, which
    // leaves the pins as set: `Output` has no `Drop` that would hand them back.
    #[cfg(feature = "xiao-external-antenna")]
    {
        use esp_hal::gpio::{Level, Output, OutputConfig};
        let _power = Output::new(peripherals.GPIO3, Level::Low, OutputConfig::default());
        delay_ms(100);
        let _select = Output::new(peripherals.GPIO14, Level::High, OutputConfig::default());
        say!("antenna: external (U.FL)");
    }

    peripherals.BT
}

/// Sleep for `ms`, yielding to the radio's tasks.
pub fn delay_ms(ms: u64) {
    CurrentThreadHandle::get().delay(Duration::from_millis(ms));
}

/// The controller as a raw HCI pipe.
pub struct Hci {
    connector: BleConnector<'static>,
    packet: [u8; PACKET_MAX],
}

impl Hci {
    pub fn new(bt: BT<'static>, config: Config) -> Option<Self> {
        Some(Self { connector: BleConnector::new(bt, config).ok()?, packet: [0; PACKET_MAX] })
    }

    /// Write `bytes` without waiting for an answer.
    pub fn write(&mut self, bytes: &[u8]) -> bool {
        self.connector.write(bytes).is_ok()
    }

    /// The next packet the controller has queued, if any.
    pub fn next(&mut self) -> Option<&[u8]> {
        match self.connector.next(&mut self.packet) {
            Ok(0) | Err(_) => None,
            Ok(read) => Some(&self.packet[..read]),
        }
    }

    /// Send one command and drain what the controller says back, handing every
    /// packet to `seen`. Returns the status the controller answered it with.
    ///
    /// `src/ble.rs`'s `command`, except that it reads the answer: the bench's
    /// first question is whether the controller accepts a command at all. It
    /// keeps draining until that answer is in and the queue has gone quiet, or
    /// the budget runs out. A Command Status is read too, since an unknown command
    /// may be refused with one.
    pub fn command(&mut self, bytes: &[u8], mut seen: impl FnMut(&[u8])) -> Option<u8> {
        let opcode = u16::from_le_bytes([bytes[1], bytes[2]]);
        if !self.write(bytes) {
            return None;
        }
        let mut status = None;
        let mut quiet = 0;
        for _ in 0..100 {
            delay_ms(POLL_MS);
            match self.next() {
                None if status.is_some() && quiet >= 1 => break,
                None => quiet += 1,
                Some(packet) => {
                    quiet = 0;
                    match (command_complete(packet), packet) {
                        (Some((op, s)), _) if op == opcode => status = Some(s),
                        // Command Status: status, command slots, opcode.
                        (_, &[0x04, 0x0F, _, s, _, lo, hi, ..])
                            if u16::from_le_bytes([lo, hi]) == opcode =>
                        {
                            status = Some(s);
                        }
                        _ => {}
                    }
                    seen(packet);
                }
            }
        }
        status
    }
}

/// A command's answer, for printing: its status byte, or `none` for no answer.
pub struct Status(pub Option<u8>);

impl core::fmt::Display for Status {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Some(status) => write!(f, "0x{status:02X}"),
            None => f.write_str("none"),
        }
    }
}

/// A MAC address in display order.
pub struct MacFmt(pub [u8; 6]);

impl core::fmt::Display for MacFmt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (i, octet) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(":")?;
            }
            write!(f, "{octet:02X}")?;
        }
        Ok(())
    }
}
