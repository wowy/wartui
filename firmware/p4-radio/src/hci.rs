//! The HCI transport under TrouBLE: `esp-radio`'s controller, watched for link-layer delivery.
//!
//! TrouBLE's own counters stop at handing a notification to the controller, which says nothing
//! about whether the central got it. [`Counting`] passes every packet through unchanged and
//! shows each to [`wartui_proto::spike::AclLedger`]: an outgoing ACL packet's header on the way
//! down, and every event on the way up, where Number Of Completed Packets names what the peer
//! acknowledged. The ledger is pure and tested on the host; this file only feeds it.
//!
//! **A packet from the controller is read twice.** The transport's packet type borrows the
//! buffer it is read into, so the bytes the ledger needs are gone into it. [`Raw`] reads the
//! packet into a buffer of its own first, the ledger looks, and the caller's type is read from
//! those bytes.
//!
//! [`Watch`] counts the transport's packets and the awaits pending on it, so a stalled host
//! shows whether the controller stopped answering. Spike diagnosis only.

use core::cell::{Cell, RefCell};
use core::convert::Infallible;

use bt_hci::ReadHciError;
use bt_hci::transport::{PacketKind, PacketToController, PacketToHost, Transport};
use embassy_time::Instant;
use embedded_io::{ErrorType, Read, ReadExactError, Write};
use embedded_io_async::Read as AsyncRead;
use wartui_proto::spike::{
    AclLedger, DISCONNECTION_COMPLETE, H4_EVENT, NUMBER_OF_COMPLETED_PACKETS,
};

/// Outgoing ACL packets the ledger tracks: above the 24 the controller buffers
/// (`esp_radio::ble::Config`, `acl_buf_count`), so it never fills.
pub const LEDGER_PACKETS: usize = 32;

/// The ledger [`Counting`] feeds.
pub type Ledger = RefCell<AclLedger<LEDGER_PACKETS>>;

/// The largest packet from the controller: what `bt_hci`'s `ExternalController` reads into.
const PACKET_MAX: usize = 259;

/// The transport's progress: packets each way, the awaits pending now, and the events that
/// return controller buffers. Each count wraps.
pub struct Watch {
    pub reads: Cell<u32>,
    pub writes: Cell<u32>,
    pub reads_in_flight: Cell<u8>,
    pub writes_in_flight: Cell<u8>,
    pub nocp_events: Cell<u32>,
    pub disconnection_events: Cell<u32>,
    /// When the latest read completed.
    pub read_done_at: Cell<Instant>,
}

impl Watch {
    pub fn new() -> Self {
        Self {
            reads: Cell::new(0),
            writes: Cell::new(0),
            reads_in_flight: Cell::new(0),
            writes_in_flight: Cell::new(0),
            nocp_events: Cell::new(0),
            disconnection_events: Cell::new(0),
            read_done_at: Cell::new(Instant::from_ticks(0)),
        }
    }
}

impl Default for Watch {
    fn default() -> Self {
        Self::new()
    }
}

/// Counts one await in flight on `cell` for as long as it lives, so a dropped future
/// uncounts too.
struct InFlight<'a>(&'a Cell<u8>);

impl<'a> InFlight<'a> {
    fn new(cell: &'a Cell<u8>) -> Self {
        cell.set(cell.get().wrapping_add(1));
        Self(cell)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get().wrapping_sub(1));
    }
}

/// Add one to `cell`, wrapping.
fn bump(cell: &Cell<u32>) {
    cell.set(cell.get().wrapping_add(1));
}

/// A transport whose traffic feeds a [`Ledger`] and a [`Watch`].
pub struct Counting<'a, T> {
    inner: T,
    ledger: &'a Ledger,
    watch: &'a Watch,
}

impl<'a, T> Counting<'a, T> {
    pub fn new(inner: T, ledger: &'a Ledger, watch: &'a Watch) -> Self {
        Self { inner, ledger, watch }
    }
}

impl<T: ErrorType> ErrorType for Counting<'_, T> {
    type Error = T::Error;
}

impl<T> Transport for Counting<'_, T>
where
    T: Transport,
    T::Error: From<ReadHciError<Infallible>>,
{
    async fn read<'a, P: PacketToHost<'a>>(&self, rx: &'a mut [u8]) -> Result<P, Self::Error> {
        let mut raw = [0; PACKET_MAX];
        let pending = InFlight::new(&self.watch.reads_in_flight);
        let read = self.inner.read(&mut raw).await;
        drop(pending);
        let Raw { kind, len } = read?;
        let watch = self.watch;
        bump(&watch.reads);
        watch.read_done_at.set(Instant::now());
        match (kind as u8, raw.first()) {
            (H4_EVENT, Some(&NUMBER_OF_COMPLETED_PACKETS)) => bump(&watch.nocp_events),
            (H4_EVENT, Some(&DISCONNECTION_COMPLETE)) => bump(&watch.disconnection_events),
            _ => {}
        }
        self.ledger.borrow_mut().on_controller_packet(kind as u8, &raw[..len]);
        let mut bytes = &raw[..len];
        Ok(P::read_hci(kind, &mut bytes, rx)?)
    }

    async fn write<P: PacketToController>(&self, tx: &P) -> Result<(), Self::Error> {
        // Before the write: the packet can complete before the write returns.
        if P::KIND == PacketKind::AclData {
            let mut head = Head::default();
            let _ = tx.write_hci(&mut head);
            self.ledger.borrow_mut().on_host_packet(P::KIND as u8, &head.bytes[..head.len]);
        }
        let pending = InFlight::new(&self.watch.writes_in_flight);
        let written = self.inner.write(tx).await;
        drop(pending);
        bump(&self.watch.writes);
        written
    }
}

/// A packet from the controller, read whole into the buffer it is given.
///
/// It reads to the end of its reader, which in `esp-radio`'s transport is the queued packet.
struct Raw {
    kind: PacketKind,
    len: usize,
}

impl<'d> PacketToHost<'d> for Raw {
    fn read_hci<R: Read>(
        kind: PacketKind,
        data: &mut R,
        buf: &'d mut [u8],
    ) -> Result<Self, ReadHciError<R::Error>> {
        let mut len = 0;
        loop {
            let n = data
                .read(&mut buf[len..])
                .map_err(|e| ReadHciError::Read(ReadExactError::Other(e)))?;
            if n == 0 {
                return Ok(Self { kind, len });
            }
            len += n;
            if len == buf.len() {
                return Ok(Self { kind, len });
            }
        }
    }

    async fn read_hci_async<R: AsyncRead>(
        kind: PacketKind,
        data: &mut R,
        buf: &'d mut [u8],
    ) -> Result<Self, ReadHciError<R::Error>> {
        let mut len = 0;
        loop {
            let n = data
                .read(&mut buf[len..])
                .await
                .map_err(|e| ReadHciError::Read(ReadExactError::Other(e)))?;
            if n == 0 {
                return Ok(Self { kind, len });
            }
            len += n;
            if len == buf.len() {
                return Ok(Self { kind, len });
            }
        }
    }
}

/// The first bytes a packet writes: an ACL packet's header.
#[derive(Default)]
struct Head {
    bytes: [u8; 4],
    len: usize,
}

impl ErrorType for Head {
    type Error = Infallible;
}

impl Write for Head {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Infallible> {
        let n = buf.len().min(self.bytes.len() - self.len);
        self.bytes[self.len..self.len + n].copy_from_slice(&buf[..n]);
        self.len += n;
        // Everything past the header is taken and dropped.
        Ok(buf.len())
    }

    fn flush(&mut self) -> Result<(), Infallible> {
        Ok(())
    }
}
