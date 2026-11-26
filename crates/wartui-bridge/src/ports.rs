//! The host's serial ports, as the operating system describes them.
//!
//! Nothing here knows the link protocol. It describes "what does the OS say a port is"
//! ([`PortCandidate`]) and the judgments made on that, which
//! [`pipe::Ports::list`](crate::pipe::Ports::list) answers and [`serial`](crate::serial) reads.
//! Listing the ports is an adapter's: `wartui-desktop`'s `serial::SerialPorts` is the desktop's.
//! The description is kept apart from the link so that a second thing looking for a device shares
//! it rather than writing another: `wartui-proto` is `no_std`, and `wartui-core` already depends on
//! this crate, so this module is reachable from either without inverting a layer.
//!
//! **A node's USB serial number is its MAC.** A node's native USB Serial/JTAG reports the
//! address its radio transmits from, so the OS has already paired a device node with the
//! board's identity before anything is opened. [`PortCandidate::mac`] is that fact, and it is
//! what lets one node be told from another without a probe, a reflash or an `esp` tool. The P4
//! bridge is reached through its board's CH343, whose serial number is the CH343's own, so a
//! bridge's address is learned only by asking it (`serial::probe`).
//!
//! **Never set DTR or RTS on a port, and never ask `serialport` to.** Opening a
//! board is safe only by accident of how the two lines move together. The kernel
//! raises `DTR | RTS` together on open and drops both together on last close. A
//! node's USB Serial/JTAG emulates the classic two-transistor auto-reset circuit, and
//! the P4 board's CH343 drives a real one into the P4's EN and GPIO35: each resets at
//! **DTR=0, RTS=1** and straps for the download ROM at **DTR=1, RTS=0**, so
//! both-together is inert.
//! `.dtr_on_open(false)` looks like politeness and is the reset sequence — it
//! moves one line and not the other. `esptool` reaches that state deliberately,
//! routing through (1,1) to avoid the other. Leave both alone, and set
//! `FlowControl::None` explicitly so the driver cannot toggle RTS on its own.
//! `wartui-desktop`'s `serial` applies the rule in its one function that opens a port, and
//! [`pipe::Ports::open`](crate::pipe::Ports::open) requires it of any adapter.
//!
//! **What a device is for is decided by its USB IDs.** The CH343's pair
//! [`CH343_VID`]:[`CH343_PID`] could be the P4 bridge, and Espressif's vendor ID is a node.
//! Three questions are asked of that, and each answer is one predicate:
//!
//! - [`could_be_a_bridge`] is every CH343. Looking for a bridge means *writing* into what
//!   is opened: the sweep and the probe send `Identify` down each CH343 until one
//!   answers. Nothing the OS reports tells a P4 bridge's CH343 from another, so asking is
//!   the only way, and a CH343 drains its UART, so asking again costs nothing. A node is never a
//!   candidate, so finding a bridge writes to no node. Exclusive opening is a backstop
//!   against two readers, not a substitute for that.
//! - [`is_wartui_board`] is every CH343 and every node: what `wartui ports` lists, and
//!   `flash-fleet` and `flash-bridge` choose from.
//! - [`could_be_a_receiver`] is everything else, so the GPS search, which listens to
//!   what it opens, never opens one of the fleet's boards. A GPS receiver behind a CH343
//!   is therefore not searched; name it with `--gps`.

use wartui_proto::mac::{self, Mac};

/// Espressif's USB vendor ID, which a node's native USB Serial/JTAG reports.
pub const ESPRESSIF_VID: u16 = 0x303A;

/// The product ID every node reports: an ESP32's native USB Serial/JTAG.
///
/// A board behind a USB-to-UART bridge chip carries the chip's vendor ID rather than
/// Espressif's, so an Espressif device that is *not* this is of some other kind, and
/// `flash-fleet` leaves it alone.
pub const SERIAL_JTAG_PID: u16 = 0x1001;

/// WCH's USB vendor ID, which the P4 board's CH343 reports.
pub const CH343_VID: u16 = 0x1A86;

/// The CH343's product ID.
pub const CH343_PID: u16 = 0x55D3;

/// A serial port the host can see, as the OS described it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortCandidate {
    /// The name to show and to open: a stable one where the OS offers it.
    pub path: String,
    /// The device node it resolves to, which is what two candidates are compared by.
    pub device: String,
    /// USB vendor ID, when the OS reported one.
    pub vid: Option<u16>,
    /// USB product ID, when the OS reported one.
    pub pid: Option<u16>,
    /// Product string, for saying which device was picked.
    pub product: Option<String>,
    /// Manufacturer string, which some receivers name themselves in.
    pub manufacturer: Option<String>,
    /// USB serial number. On a node this is the board's MAC; on a CH343 it is the CH343's own.
    pub serial: Option<String>,
}

impl PortCandidate {
    /// The board's address, when its serial number is one.
    #[must_use]
    pub fn mac(&self) -> Option<Mac> {
        self.serial.as_deref().and_then(mac::parse)
    }

    /// The board's address if it reported one, else its path.
    #[must_use]
    pub fn label(&self) -> String {
        self.mac().map_or_else(|| self.path.clone(), |address| mac::full(&address).to_string())
    }
}

/// Build a candidate by hand.
///
/// The normalization from `serialport`'s own types is deliberately private to
/// `wartui-desktop`'s `serial`, so that nothing else names `UsbPortInfo` — its shape depends
/// on Cargo features any dependent crate can turn on, and a test written against it would
/// break for a reason that has nothing to do with wartui.
#[must_use]
pub fn candidate(
    device: &str,
    vid: Option<u16>,
    pid: Option<u16>,
    serial: Option<&str>,
) -> PortCandidate {
    PortCandidate {
        path: device.to_owned(),
        device: device.to_owned(),
        vid,
        pid,
        product: None,
        manufacturer: None,
        serial: serial.map(str::to_owned),
    }
}

/// Whether this is a CH343, which the P4 bridge is reached through.
#[must_use]
pub fn is_ch343(candidate: &PortCandidate) -> bool {
    candidate.vid == Some(CH343_VID) && candidate.pid == Some(CH343_PID)
}

/// Whether this port could be the bridge.
#[must_use]
pub fn could_be_a_bridge(candidate: &PortCandidate) -> bool {
    is_ch343(candidate) && is_usable_path(&candidate.path)
}

/// Whether this is one of the fleet's boards: a node, or a CH343 that may be the P4 bridge.
#[must_use]
pub fn is_wartui_board(candidate: &PortCandidate) -> bool {
    candidate.vid == Some(ESPRESSIF_VID) || is_ch343(candidate)
}

/// Whether this port could be an NMEA receiver.
///
/// Every usable path but the fleet's boards, and that is the whole rule: a receiver is
/// recognized by what it says when read, not by what it is called.
#[must_use]
pub fn could_be_a_receiver(candidate: &PortCandidate) -> bool {
    is_usable_path(&candidate.path) && !is_wartui_board(candidate)
}

/// On macOS every USB serial device appears twice. `/dev/tty.*` is the callout
/// side and blocks on carrier detect, so only `/dev/cu.*` is usable.
#[must_use]
pub fn is_usable_path(path: &str) -> bool {
    !path.starts_with("/dev/tty.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_scanner_partitions_bridge_node_and_gps_ports_when_enumerating_usb_devices() {
        let bridge = ch343("/dev/ttyACM2");
        let node = candidate("/dev/ttyACM0", Some(ESPRESSIF_VID), Some(0x1001), None);
        let puck = candidate("/dev/ttyACM1", Some(0x1546), Some(0x01A7), None);
        assert!(could_be_a_bridge(&bridge) && is_wartui_board(&bridge));
        assert!(!could_be_a_receiver(&bridge));
        // Finding a bridge writes, so a node is never a candidate, and the GPS search never
        // opens it either: it stays listed only as one of the fleet's boards.
        assert!(!could_be_a_bridge(&node) && is_wartui_board(&node));
        assert!(!could_be_a_receiver(&node));
        assert!(could_be_a_receiver(&puck) && !could_be_a_bridge(&puck));
        assert!(!is_wartui_board(&puck));
    }

    fn ch343(device: &str) -> PortCandidate {
        candidate(device, Some(CH343_VID), Some(CH343_PID), Some("AAAAAAAAAA"))
    }

    #[test]
    fn ports_could_be_a_bridge_when_port_is_a_ch343() {
        let c = ch343("/dev/ttyACM2");
        assert!(is_ch343(&c) && could_be_a_bridge(&c) && !could_be_a_receiver(&c));
        // Its serial number is the CH343's, so it names no board.
        assert_eq!(c.mac(), None);
    }

    #[test]
    fn ports_not_a_bridge_when_wch_vendor_has_another_product() {
        let mut ch340 = ch343("/dev/ttyUSB0");
        ch340.pid = Some(0x7523);
        assert!(!is_ch343(&ch340) && !could_be_a_bridge(&ch340) && could_be_a_receiver(&ch340));
        assert!(!is_wartui_board(&ch340));
    }

    #[test]
    fn ports_keeps_bridge_and_receiver_apart_when_given_any_port() {
        let mut tty = ch343("/dev/ttyACM2");
        tty.path = "/dev/tty.usbserial1".to_owned();
        let table = [
            ch343("/dev/ttyACM2"),
            tty,
            candidate("/dev/ttyACM0", Some(ESPRESSIF_VID), Some(SERIAL_JTAG_PID), None),
            candidate("/dev/ttyACM1", Some(0x1546), Some(0x01A7), None),
            candidate("/dev/ttyACM3", None, None, None),
            candidate("/dev/tty.usbmodem2", Some(ESPRESSIF_VID), Some(SERIAL_JTAG_PID), None),
        ];
        for c in &table {
            let usable = is_usable_path(&c.path);
            assert_eq!(could_be_a_receiver(c), usable && !is_wartui_board(c), "{c:?}");
            assert!(!(could_be_a_bridge(c) && could_be_a_receiver(c)), "{c:?}");
            assert!(!could_be_a_bridge(c) || is_wartui_board(c), "{c:?}");
        }
    }
}
