//! The desktop adapters under the link drivers: [`SerialPorts`], the host's serial ports through
//! the `serialport` crate.
//!
//! This is the one module in the crate that names `serialport`, so the driver above it
//! ([`serial`](crate::serial)) runs on any adapter that keeps [`Ports`]' contract.

use std::io::{self, Read, Write};
use std::sync::Arc;

use wartui_proto::link::HOST_BAUD;

use crate::TransportError;
use crate::pipe::{Abort, Ports, READ_TIMEOUT, SerialParts, SerialPipe};
use crate::ports::{self, PortCandidate, StableNames};

/// The host's serial ports, as `serialport` lists and opens them.
#[derive(Debug, Clone, Copy, Default)]
pub struct SerialPorts;

/// [`SerialPorts`], shared the way the transports take it.
#[must_use]
pub fn serial_ports() -> Arc<dyn Ports> {
    Arc::new(SerialPorts)
}

impl Ports for SerialPorts {
    fn list(&self) -> Result<Vec<PortCandidate>, TransportError> {
        let ports =
            serialport::available_ports().map_err(|e| TransportError::Enumerate(e.into()))?;
        Ok(ports::with_stable_paths(from_port_infos(ports), &StableNames::read()))
    }

    fn open(&self, candidate: &PortCandidate) -> Result<Box<dyn SerialPipe>, TransportError> {
        let path = &candidate.path;
        let port = serialport::new(path, HOST_BAUD)
            .timeout(READ_TIMEOUT)
            // Named rather than left to the default, because it is the setting that
            // keeps the driver from moving RTS on its own; `ports` explains why that
            // matters on a board whose reset line is a pair of modem signals.
            .flow_control(serialport::FlowControl::None)
            .open()
            .map_err(|e| open_failure(path, &e))?;
        Ok(Box::new(Pipe(port)))
    }
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
    use super::{SerialPorts, open_failure};
    use crate::TransportError;
    use crate::pipe::Ports;
    use crate::ports::candidate;

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
