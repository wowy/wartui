//! The host's serial ports through the `serialport` crate: [`SerialPorts`], the adapter under the
//! host link ([`Ports`]) and the GPS search ([`ReceiverPorts`]), and [`open_reader`] for a caller
//! that only listens.
//!
//! **Every port is opened by one function, `open_port`, which sets `FlowControl::None` and never
//! sets DTR or RTS.** Moving one modem line without the other is the reset sequence on a P4 board
//! and on a node ([`wartui_bridge::ports`] has why), so nothing here opens a port any other way.
//!
//! **A port is named by the most stable path the OS offers** ([`with_stable_paths`]): a udev
//! `by-id` name where it means one device, else its `by-path` name, else the device node. Both
//! the host link and the GPS search list through the same enumeration.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use wartui_bridge::TransportError;
use wartui_bridge::pipe::{Abort, Ports, READ_TIMEOUT, SerialParts, SerialPipe};
use wartui_bridge::ports::{self, PortCandidate};
use wartui_core::gps::ReceiverPorts;
use wartui_proto::link::HOST_BAUD;

/// Where udev keeps the names that survive a replug.
const BY_ID: &str = "/dev/serial/by-id";
const BY_PATH: &str = "/dev/serial/by-path";

/// The host's serial ports, as `serialport` lists and opens them.
#[derive(Debug, Clone, Copy, Default)]
pub struct SerialPorts;

/// [`SerialPorts`], shared the way the transports take it.
#[must_use]
pub fn serial_ports() -> Arc<dyn Ports> {
    Arc::new(SerialPorts)
}

/// [`SerialPorts`], shared the way the GPS reader takes it.
#[must_use]
pub fn receiver_ports() -> Arc<dyn ReceiverPorts> {
    Arc::new(SerialPorts)
}

impl Ports for SerialPorts {
    fn list(&self) -> Result<Vec<PortCandidate>, TransportError> {
        list().map_err(|e| TransportError::Enumerate(e.into()))
    }

    fn open(&self, candidate: &PortCandidate) -> Result<Box<dyn SerialPipe>, TransportError> {
        let path = &candidate.path;
        let port = open_port(path, HOST_BAUD, READ_TIMEOUT).map_err(|e| open_failure(path, &e))?;
        Ok(Box::new(Pipe(port)))
    }
}

impl ReceiverPorts for SerialPorts {
    fn list(&self) -> io::Result<Vec<PortCandidate>> {
        Ok(list()?)
    }

    fn open(&self, path: &str, baud: u32) -> io::Result<Box<dyn Read + Send>> {
        open_reader(path, baud, wartui_core::gps::READ_TIMEOUT)
    }
}

/// Open `path` at `baud` to listen, each read waiting up to `timeout`.
///
/// # Errors
/// The OS's reason the port would not open.
pub fn open_reader(path: &str, baud: u32, timeout: Duration) -> io::Result<Box<dyn Read + Send>> {
    Ok(Box::new(open_port(path, baud, timeout)?))
}

/// Open a serial port the one way this crate opens one.
fn open_port(
    path: &str,
    baud: u32,
    timeout: Duration,
) -> serialport::Result<Box<dyn serialport::SerialPort>> {
    serialport::new(path, baud)
        .timeout(timeout)
        // Named rather than left to the default, because it is the setting that
        // keeps the driver from moving RTS on its own; `wartui_bridge::ports` explains
        // why that matters on a board whose reset line is a pair of modem signals.
        .flow_control(serialport::FlowControl::None)
        .open()
}

/// Every USB serial port attached, named by the most stable path the OS offers.
fn list() -> serialport::Result<Vec<PortCandidate>> {
    let ports = serialport::available_ports()?;
    Ok(with_stable_paths(from_port_infos(ports), &StableNames::read()))
}

/// The OS's reason a port would not open, with the remedy for the one that has one.
///
/// Permission is the failure worth separating: on Linux a serial port belongs to
/// `dialout`, and "no bridge found" is exactly the wrong thing to say about a board
/// that was found and refused.
fn open_failure(path: &str, error: &serialport::Error) -> TransportError {
    if matches!(error.kind(), serialport::ErrorKind::Io(io::ErrorKind::PermissionDenied)) {
        return TransportError::Forbidden { port: path.to_owned() };
    }
    TransportError::Open { port: path.to_owned(), source: error.clone().into() }
}

/// `serialport`'s enumeration, in [`ports`]' terms. Non-USB ports are dropped:
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
        .filter(|candidate| ports::is_usable_path(&candidate.device))
        .collect();
    found.sort_by(|a, b| a.device.cmp(&b.device));
    found
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

/// An open `serialport` port.
struct Pipe(Box<dyn serialport::SerialPort>);

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Pipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl SerialPipe for Pipe {
    fn split(self: Box<Self>) -> io::Result<SerialParts> {
        let writer = self.0.try_clone()?;
        let abort = self.0.try_clone()?;
        Ok(SerialParts {
            reader: Box::new(self.0),
            writer: Box::new(writer),
            abort: Box::new(Clear(abort)),
        })
    }
}

/// The handle that empties the port's output queue. The handles share one open file
/// description, so this releases a write blocked on another.
struct Clear(Box<dyn serialport::SerialPort>);

impl Abort for Clear {
    fn clear_output(&mut self) {
        let _ = self.0.clear(serialport::ClearBuffer::Output);
    }
}

#[cfg(test)]
mod tests {
    use wartui_bridge::TransportError;
    use wartui_bridge::pipe::Ports;
    use wartui_bridge::ports::candidate;

    use super::{SerialPorts, open_failure};

    #[test]
    fn desktop_ports_report_open_error_when_path_does_not_exist() {
        let missing = std::env::temp_dir().join("wartui-desktop-test-missing").join("ttyACM0");
        let path = missing.to_str().expect("a UTF-8 temp dir");
        let opened = SerialPorts.open(&candidate(path, None, None, None));
        assert!(
            matches!(&opened, Err(TransportError::Open { port, .. }) if port == path),
            "{:?}",
            opened.err()
        );
    }

    #[test]
    fn desktop_ports_word_refusal_as_forbidden_when_open_is_denied() {
        // A port that belongs to `dialout` was found and refused, which "no bridge found" is not.
        let denied = serialport::Error::new(
            serialport::ErrorKind::Io(std::io::ErrorKind::PermissionDenied),
            "Permission denied",
        );
        let refused = open_failure("/dev/ttyACM0", &denied);
        assert!(
            matches!(&refused, TransportError::Forbidden { port } if port == "/dev/ttyACM0"),
            "{refused:?}"
        );
    }
}
