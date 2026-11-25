//! The byte pipes under the link drivers.
//!
//! A link driver is the protocol: which ports it opens, what it writes into them, how long it
//! waits and when it gives up ([`serial`](crate::serial)). An adapter is the OS: how a port is
//! listed, opened and read. The two meet here, so the driver is the same code whatever reaches
//! the ports, and is tested against a fake rather than a board. [`desktop`](crate::desktop)
//! holds the adapter built on the `serialport` crate.
//!
//! **What a [`Ports`] adapter owes the driver**, which relies on each point:
//!
//! - [`Ports::open`] opens at [`HOST_BAUD`] with no flow control, and **never sets DTR or RTS**.
//!   Moving one modem line without the other resets a P4 board, and a node's USB Serial/JTAG
//!   reads it the same way ([`ports`](crate::ports) has why). A refusal for permission is
//!   [`TransportError::Forbidden`], so the operator is told about `dialout` rather than that no
//!   bridge was found; any other is [`TransportError::Open`].
//! - A read or a write returns within about [`READ_TIMEOUT`], failing with
//!   [`ErrorKind::TimedOut`] when nothing moved. The driver's threads poll a stop flag between
//!   calls, so a call that blocks longer delays the end of a connection by as much.
//! - A port that is gone fails its reads and writes with an error. A read returning `Ok(0)`
//!   means nothing arrived, and the driver reads again at once, so an adapter must not report an
//!   unplugged device as end-of-file.
//! - [`Ports::list`] and [`Ports::open`] block. The driver calls both from its async task (only
//!   a probe moves the open to a blocking thread), so each must return promptly, as a tty open
//!   does.
//! - The three [`SerialParts`] are handles on one port. The port closes once all three are
//!   dropped, and [`Abort::clear_output`] on one releases a write blocked on another.
//!
//! [`HOST_BAUD`]: wartui_proto::link::HOST_BAUD
//! [`ErrorKind::TimedOut`]: std::io::ErrorKind::TimedOut

use std::time::Duration;

use crate::TransportError;
use crate::ports::PortCandidate;

/// The longest a read or a write on a [`SerialPipe`] blocks before failing with `TimedOut`.
///
/// Long enough not to spin, short enough that shutdown feels immediate.
pub const READ_TIMEOUT: Duration = Duration::from_millis(50);

/// The host's serial ports: what is attached, and a way to open one for the host link.
pub trait Ports: std::fmt::Debug + Send + Sync {
    /// Every USB serial port attached, named by the most stable path the OS offers.
    ///
    /// # Errors
    /// [`TransportError::Enumerate`] if the ports cannot be listed.
    fn list(&self) -> Result<Vec<PortCandidate>, TransportError>;

    /// Open `candidate.path` for the host link.
    ///
    /// # Errors
    /// [`TransportError::Forbidden`] if the OS refused it for permission, else
    /// [`TransportError::Open`].
    fn open(&self, candidate: &PortCandidate) -> Result<Box<dyn SerialPipe>, TransportError>;
}

/// One open port, read and written as bytes.
pub trait SerialPipe: std::io::Read + std::io::Write + Send {
    /// The port as three handles: one to read, one to write, and one to abort a write.
    ///
    /// # Errors
    /// The OS's reason a handle could not be made.
    fn split(self: Box<Self>) -> std::io::Result<SerialParts>;
}

/// The handles [`SerialPipe::split`] makes, all on one port.
pub struct SerialParts {
    /// The reader thread's handle.
    pub reader: Box<dyn std::io::Read + Send>,
    /// The writer thread's handle.
    pub writer: Box<dyn std::io::Write + Send>,
    /// The handle that ends a connection.
    pub abort: Box<dyn Abort>,
}

/// What ends a connection's writes.
pub trait Abort: Send {
    /// Empty the port's output queue, which releases a write blocked in it.
    fn clear_output(&mut self);
}
