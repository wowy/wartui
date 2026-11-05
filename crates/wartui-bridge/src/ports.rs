//! The host's serial ports, as the operating system describes them.
//!
//! Nothing here knows the link protocol. It answers "what is attached, and what
//! does the OS say it is", which is [`serial`](crate::serial)'s input. It is kept
//! separate from the judgment made on that answer so that a second thing looking
//! for a device shares one enumeration rather than writing another: `wartui-proto`
//! is `no_std` and cannot hold `serialport`, and `wartui-core` already depends on
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
//!
//! **What a device is for is decided by its USB IDs.** The CH343's pair
//! [`CH343_VID`]:[`CH343_PID`] could be the P4 bridge, and Espressif's vendor ID is a node.
//! Three questions are asked of that, and each answer is one predicate:
//!
//! - [`could_be_a_bridge`] is every CH343. Looking for a bridge means *writing* into what
//!   is opened: the sweep and the probe send one `Identify` down each CH343 until one
//!   answers. Nothing the OS reports tells a P4 bridge's CH343 from another, so asking is
//!   the only way, and one frame is the least a question can write. A node is never a
//!   candidate, so finding a bridge writes to no node. Exclusive opening is a backstop
//!   against two readers, not a substitute for that.
//! - [`is_wartui_board`] is every CH343 and every node: what `wartui ports` lists, and
//!   `flash-fleet` and `flash-bridge` choose from.
//! - [`could_be_a_receiver`] is everything else, so the GPS search, which listens to
//!   what it opens, never opens one of the fleet's boards. A GPS receiver behind a CH343
//!   is therefore not searched; name it with `--gps`.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use wartui_proto::mac::{self, Mac};

use crate::TransportError;

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

/// Where udev keeps the names that survive a replug.
const BY_ID: &str = "/dev/serial/by-id";
const BY_PATH: &str = "/dev/serial/by-path";

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
/// The normalization from `serialport`'s own types is deliberately private, so that
/// nothing outside this module names `UsbPortInfo` — its shape depends on Cargo
/// features any dependent crate can turn on, and a test written against it would
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

/// Every USB serial port attached, named by the most stable path the OS offers.
///
/// # Errors
/// [`TransportError::Enumerate`] if the ports cannot be listed.
pub fn list() -> Result<Vec<PortCandidate>, TransportError> {
    let ports = serialport::available_ports().map_err(TransportError::Enumerate)?;
    Ok(with_stable_paths(from_port_infos(ports), &StableNames::read()))
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

/// The stable names udev keeps for a device node.
///
/// Two directories rather than one because they fail differently. `by-id` is built
/// from what the device says about itself, so it is the name worth showing — but a
/// device that reports no serial number shares its name with every other of its
/// model, and udev can only give it to one of them. `by-path` names the socket
/// instead, which is unique whatever the device claims, and stable for as long as
/// the cable stays where it is.
#[derive(Debug, Default)]
pub struct StableNames {
    by_id: HashMap<String, String>,
    by_path: HashMap<String, String>,
}

impl StableNames {
    /// What udev has recorded. Empty where the directories do not exist, which is
    /// every non-Linux host and a Linux one with no serial device attached.
    #[must_use]
    pub fn read() -> Self {
        Self { by_id: links_in(BY_ID), by_path: links_in(BY_PATH) }
    }

    /// The same thing from `(device, link)` pairs, so the choosing can be tested
    /// without a `/dev` to read.
    #[must_use]
    pub fn from_pairs(
        by_id: impl IntoIterator<Item = (String, String)>,
        by_path: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        Self { by_id: by_id.into_iter().collect(), by_path: by_path.into_iter().collect() }
    }

    /// The best name for `device`, given whether its `by-id` name can be trusted to
    /// mean this device and no other.
    #[must_use]
    pub fn best<'a>(&'a self, device: &'a str, by_id_is_its_own: bool) -> &'a str {
        if by_id_is_its_own && let Some(link) = self.by_id.get(device) {
            return link;
        }
        self.by_path.get(device).map_or(device, String::as_str)
    }
}

/// Name each candidate by the most stable path that means it alone.
///
/// A `by-id` name is built from the vendor, product and serial number, so two
/// devices reporting the same three are one name between them — udev gives it to
/// whichever enumerated last, and a run that opened it would be opening whichever
/// was plugged in most recently rather than the one it meant. Those are named by
/// socket instead.
#[must_use]
pub fn with_stable_paths(
    candidates: Vec<PortCandidate>,
    names: &StableNames,
) -> Vec<PortCandidate> {
    let ambiguous = indistinct_identities(&candidates);
    candidates
        .into_iter()
        .map(|candidate| {
            let own = !ambiguous.contains(&identity(&candidate));
            let path = names.best(&candidate.device, own).to_owned();
            PortCandidate { path, ..candidate }
        })
        .collect()
}

/// What udev builds a `by-id` name out of.
fn identity(candidate: &PortCandidate) -> (Option<u16>, Option<u16>, Option<String>) {
    (candidate.vid, candidate.pid, candidate.serial.clone())
}

/// The identities more than one attached device answers to.
fn indistinct_identities(
    candidates: &[PortCandidate],
) -> HashSet<(Option<u16>, Option<u16>, Option<String>)> {
    let mut seen = HashSet::new();
    let mut twice = HashSet::new();
    for candidate in candidates {
        let id = identity(candidate);
        if !seen.insert(id.clone()) {
            twice.insert(id);
        }
    }
    twice
}

/// Every symlink in `dir`, keyed by the device node it resolves to.
fn links_in(dir: &str) -> HashMap<String, String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return HashMap::new() };
    let mut links = HashMap::new();
    for entry in entries.flatten() {
        let link = entry.path();
        let Ok(device) = std::fs::canonicalize(&link) else { continue };
        let (Some(device), Some(link)) = (to_str(&device), to_str(&link)) else { continue };
        // Sorted rather than last-wins so that two names for one device — `-if00`
        // and a `by-path` alias — resolve the same way on every run.
        links
            .entry(device)
            .and_modify(|held: &mut String| {
                if link < *held {
                    *held = link.clone();
                }
            })
            .or_insert(link);
    }
    links
}

fn to_str(path: &Path) -> Option<String> {
    path.to_str().map(str::to_owned)
}

/// `serialport`'s enumeration, in this module's terms. Non-USB ports are dropped:
/// nothing wartui talks to arrives over a motherboard serial header.
fn from_port_infos(ports: Vec<serialport::SerialPortInfo>) -> Vec<PortCandidate> {
    let mut found: Vec<PortCandidate> = ports
        .into_iter()
        .filter_map(|port| match port.port_type {
            serialport::SerialPortType::UsbPort(usb) => Some(PortCandidate {
                path: port.port_name.clone(),
                device: port.port_name,
                vid: Some(usb.vid),
                pid: Some(usb.pid),
                product: usb.product,
                manufacturer: usb.manufacturer,
                serial: usb.serial_number,
            }),
            _ => None,
        })
        .filter(|candidate| is_usable_path(&candidate.device))
        .collect();
    found.sort_by(|a, b| a.device.cmp(&b.device));
    found
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
        candidate(device, Some(CH343_VID), Some(CH343_PID), Some("5CF7153520"))
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
