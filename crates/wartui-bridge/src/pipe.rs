//! The byte pipes under the link drivers.
//!
//! A link driver is the protocol: which ports or bridges it opens, what it writes into them, how
//! long it waits and when it gives up ([`serial`](crate::serial), [`ble`](crate::ble)). An adapter
//! is the OS: how a port is listed, opened and read, or how a radio scans, connects and
//! subscribes. The two meet here, so each driver is the same code whatever is under it, and is
//! tested against a fake rather than a board. [`desktop`](crate::desktop) holds the adapters built
//! on the `serialport` and `btleplug` crates.
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
//! **What a [`Radio`] adapter owes the driver**, which relies on each point:
//!
//! - A [`Sighting`] is only of a device carrying the id (`wartui_proto::ble::ID_UUID`). `live`
//!   means heard advertising in this scan, not a replay of the OS's listing. `peer` is stable
//!   across scans and process runs, since [`BridgeMemory`](crate::remember::BridgeMemory) keeps it.
//! - Any call may pend. The driver times each, since a CoreBluetooth connect to a bridge that
//!   dropped this host's bond never completes, and drops a future it gave up on.
//!   [`Radio::abandon`] then cancels the pending connect, and a [`Connection`] whose call was
//!   dropped still takes [`Connection::close`].
//! - The notifications stream is opened before the subscription is asked for, so nothing sent on
//!   its answer is missed. A notification ready beside a drop is yielded first.
//!   [`Incoming::Dropped`] comes at most once, with kind [`RadioErrorKind::Disconnected`] for the
//!   peer's own drop.
//! - [`RadioErrorKind::NotBonded`] is the peer's ATT Insufficient Authentication or Encryption
//!   answer to a subscription or a write request.
//! - [`Connection::close`] does not disconnect a link that already dropped.
//! - [`Connection::mtu`] is the ATT MTU, so an OS that reports a maximum write length adds the
//!   3-byte ATT header.
//!
//! [`HOST_BAUD`]: wartui_proto::link::HOST_BAUD
//! [`ErrorKind::TimedOut`]: std::io::ErrorKind::TimedOut

use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;

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

/// What the driver needs to know of the OS under a [`Radio`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RadioTraits {
    /// Whether the OS pairs on the first connection, behind a dialog of its own. BlueZ pairs
    /// beforehand.
    pub pairs_on_connect: bool,
    /// Whether the OS is macOS, whose dialog and settings the driver's notices name.
    pub macos: bool,
}

/// A radio call that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadioError {
    /// What the driver acts on.
    pub kind: RadioErrorKind,
    /// The stack's words, for the operator.
    pub text: String,
}

impl RadioError {
    /// A failure of `kind`, in the words `text`.
    #[must_use]
    pub fn new(kind: RadioErrorKind, text: impl Into<String>) -> Self {
        Self { kind, text: text.into() }
    }
}

impl std::fmt::Display for RadioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl std::error::Error for RadioError {}

/// What kind of failure a [`RadioError`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadioErrorKind {
    /// The OS has no Bluetooth adapter to use.
    NoAdapter,
    /// The peer's ATT refusal of a link not encrypted with a bond it holds.
    NotBonded,
    /// The peer's link dropped.
    Disconnected,
    /// The peer offers no wartui link service.
    NoService,
    /// Anything else, in the stack's words.
    Other,
}

/// A bridge heard or listed: a device carrying the id (`wartui_proto::ble::ID_UUID`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sighting {
    /// The key the OS knows it by, which [`BridgeMemory`](crate::remember::BridgeMemory) stores.
    pub peer: String,
    /// The last two octets its id carries.
    pub octets: [u8; 2],
    /// The name it advertises, when the OS heard one: a label, never matched.
    pub name: Option<String>,
    /// Heard advertising now, rather than only listed.
    pub live: bool,
}

/// A peer the OS knows without a scan ([`Radio::recall`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recalled {
    /// The key the OS knows it by.
    pub peer: String,
    /// The last two octets its id carries, when the OS has them.
    pub octets: Option<[u8; 2]>,
}

/// A link [`Radio::connect`] holds.
pub struct Connected {
    /// The link.
    pub connection: Box<dyn Connection>,
    /// Whether this call made the link, rather than finding one the OS already held.
    pub made: bool,
}

impl std::fmt::Debug for Connected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connected").field("made", &self.made).finish_non_exhaustive()
    }
}

/// What a subscription to Up carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    /// One notification's value.
    Notified(Vec<u8>),
    /// The link dropped.
    Dropped(RadioError),
}

/// The host's Bluetooth radio: what it hears, and a way to connect to a bridge.
pub trait Radio: std::fmt::Debug + Send + Sync {
    /// What the driver needs to know of the OS under it.
    fn traits(&self) -> RadioTraits;

    /// The peer the OS knows by `key` without a scan, if it lists one.
    fn recall<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Recalled>, RadioError>>;

    /// Start an unfiltered scan. The stream yields each bridge as it is heard, again when what is
    /// known of it changes, and ends only if the adapter stops reporting.
    fn scan(&self) -> BoxFuture<'_, Result<BoxStream<'static, Sighting>, RadioError>>;

    /// Stop the scan.
    fn stop_scan(&self) -> BoxFuture<'_, ()>;

    /// The bridges the OS lists now, heard by this scan or not.
    fn known(&self) -> BoxFuture<'_, Result<Vec<Sighting>, RadioError>>;

    /// Connect to `peer`, a key a [`Sighting`] or [`Recalled`] gave.
    fn connect<'a>(&'a self, peer: &'a str) -> BoxFuture<'a, Result<Connected, RadioError>>;

    /// Cancel a connect to `peer` that has not completed.
    fn abandon<'a>(&'a self, peer: &'a str) -> BoxFuture<'a, ()>;
}

/// One link to a bridge.
pub trait Connection: Send + Sync {
    /// Find the link service's Up and Down. [`RadioErrorKind::NoService`] when the peer offers
    /// none.
    fn discover(&mut self) -> BoxFuture<'_, Result<(), RadioError>>;

    /// Subscribe to Up. The stream carries Up's notifications and the link's drop.
    fn subscribe(&mut self) -> BoxFuture<'_, Result<BoxStream<'static, Incoming>, RadioError>>;

    /// The ATT MTU, 23 when the OS does not say.
    fn mtu(&self) -> u16;

    /// Write `bytes` to Down.
    fn write<'a>(
        &'a mut self,
        bytes: &'a [u8],
        kind: WriteKind,
    ) -> BoxFuture<'a, Result<(), RadioError>>;

    /// Whether the OS still holds the link.
    fn is_connected(&self) -> BoxFuture<'_, bool>;

    /// Let the peer go: unsubscribe from Up when `unsubscribe`, then disconnect a link that is
    /// still up.
    fn close(self: Box<Self>, unsubscribe: bool) -> BoxFuture<'static, ()>;
}

/// How a write to Down goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKind {
    /// A write without response: the central waits on nothing.
    Command,
    /// A write request, which the C6 answers once it has forwarded it.
    Request,
}
