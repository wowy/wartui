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

use core::cell::RefCell;
use core::convert::Infallible;

use bt_hci::ReadHciError;
use bt_hci::transport::{PacketKind, PacketToController, PacketToHost, Transport};
use embedded_io::{ErrorType, Read, ReadExactError, Write};
use embedded_io_async::Read as AsyncRead;
use wartui_proto::spike::AclLedger;

/// Outgoing ACL packets the ledger tracks: above the 24 the controller buffers
/// (`esp_radio::ble::Config`, `acl_buf_count`), so it never fills.
pub const LEDGER_PACKETS: usize = 32;

/// The ledger [`Counting`] feeds.
pub type Ledger = RefCell<AclLedger<LEDGER_PACKETS>>;

/// The largest packet from the controller: what `bt_hci`'s `ExternalController` reads into.
const PACKET_MAX: usize = 259;

/// A transport whose traffic feeds a [`Ledger`].
pub struct Counting<'a, T> {
    inner: T,
    ledger: &'a Ledger,
}

impl<'a, T> Counting<'a, T> {
    pub fn new(inner: T, ledger: &'a Ledger) -> Self {
        Self { inner, ledger }
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
        let Raw { kind, len } = self.inner.read(&mut raw).await?;
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
        self.inner.write(tx).await
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
