//! The host's serial ports, as the operating system describes them.
//!
//! Nothing here knows the link protocol. It answers "what is attached, and what
//! does the OS say it is" — which is [`serial`](crate::serial)'s input, and also
//! the GPS reader's in `wartui-core`. That is why generic port enumeration lives
//! in this crate rather than beside either consumer: `wartui-proto` is `no_std`
//! and cannot hold `serialport`, and `wartui-core` already depends on this crate,
//! so a module here is reachable from both without inverting a layer.
//!
//! **An ESP32's USB serial number is its MAC.** The C5's and C6's native USB
//! Serial/JTAG reports the address the radio transmits from, so the OS has
//! already paired a device node with the board's identity before anything is
//! opened. [`PortCandidate::mac`] is that fact, and it is what lets one board be
//! told from another without a probe, a reflash or an `esp` tool.
//!
//! **A candidate set is decided by vendor ID and nothing else.**
//! [`could_be_a_bridge`] and [`could_be_a_receiver`] partition what is attached,
//! and they are complements on purpose: whoever is looking for a bridge writes
//! into what it opens, and a node's port is the one thing that must never be
//! written into by something looking for a GPS. Exclusive opening is a backstop
//! against two readers, not a substitute for the partition.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use wartui_proto::link::Mac;

use crate::TransportError;

/// Espressif's USB vendor ID, shared by the C5's and C6's native USB Serial/JTAG.
pub const ESPRESSIF_VID: u16 = 0x303A;

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
    /// USB serial number. On an ESP32 this is the board's MAC.
    pub serial: Option<String>,
}

impl PortCandidate {
    /// The board's address, when its serial number is one.
    #[must_use]
    pub fn mac(&self) -> Option<Mac> {
        self.serial.as_deref().and_then(parse_mac)
    }
}

/// Build a candidate by hand.
///
/// The normalisation from `serialport`'s own types is deliberately private, so that
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

/// Whether this port could be the bridge — or a node, which looks identical.
#[must_use]
pub fn could_be_a_bridge(candidate: &PortCandidate) -> bool {
    candidate.vid == Some(ESPRESSIF_VID) && is_usable_path(&candidate.path)
}

/// Whether this port could be an NMEA receiver.
///
/// The complement of [`could_be_a_bridge`], and that is the whole rule: a receiver
/// is recognised by what it says when read, not by what it is called.
#[must_use]
pub fn could_be_a_receiver(candidate: &PortCandidate) -> bool {
    candidate.vid != Some(ESPRESSIF_VID) && is_usable_path(&candidate.path)
}

/// On macOS every USB serial device appears twice. `/dev/tty.*` is the callout
/// side and blocks on carrier detect, so only `/dev/cu.*` is usable.
#[must_use]
pub fn is_usable_path(path: &str) -> bool {
    !path.starts_with("/dev/tty.")
}

/// Read a MAC written the way an ESP32's USB serial number and this crate's own
/// output both write it: six colon-separated hex pairs.
#[must_use]
pub fn parse_mac(text: &str) -> Option<Mac> {
    let mut mac = [0u8; 6];
    let mut octets = text.split(':');
    for slot in &mut mac {
        let octet = octets.next()?;
        if octet.len() != 2 {
            return None;
        }
        *slot = u8::from_str_radix(octet, 16).ok()?;
    }
    octets.next().is_none().then_some(mac)
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
    fn an_esp32_serial_number_reads_as_the_address_it_transmits_from() {
        assert_eq!(parse_mac("10:BD:A3:EC:44:C0"), Some([0x10, 0xBD, 0xA3, 0xEC, 0x44, 0xC0]));
        assert_eq!(parse_mac("10:bd:a3:ec:44:c0"), Some([0x10, 0xBD, 0xA3, 0xEC, 0x44, 0xC0]));
    }

    #[test]
    fn a_manufacturing_serial_is_not_an_address() {
        // Everything on the bus that is not an ESP32 carries one of these.
        assert_eq!(parse_mac("0001"), None);
        assert_eq!(parse_mac(""), None);
        assert_eq!(parse_mac("10:BD:A3:EC:44"), None, "five octets");
        assert_eq!(parse_mac("10:BD:A3:EC:44:C0:FF"), None, "seven");
        assert_eq!(parse_mac("10:BD:A3:EC:44:CG"), None, "not hex");
        assert_eq!(parse_mac("1:BD:A3:EC:44:C0"), None, "an octet written short");
    }

    #[test]
    fn the_two_candidate_sets_never_overlap() {
        let bridge = candidate("/dev/ttyACM0", Some(ESPRESSIF_VID), Some(0x1001), None);
        let puck = candidate("/dev/ttyACM1", Some(0x1546), Some(0x01A7), None);
        assert!(could_be_a_bridge(&bridge) && !could_be_a_receiver(&bridge));
        assert!(could_be_a_receiver(&puck) && !could_be_a_bridge(&puck));
    }
}
