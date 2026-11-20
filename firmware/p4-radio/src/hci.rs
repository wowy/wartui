//! The HCI transport under TrouBLE: esp-radio's connector, with every command encoded by bt-hci's
//! sync encoder.
//!
//! bt-hci 0.10.1's async encoder for a byte slice writes the slice's length plus one as its length
//! byte (`param/primitives.rs`, `write_hci_async`); its sync encoder writes the length itself.
//! esp-radio's connector writes every packet through the async one. A command whose parameters
//! hold a byte slice therefore reaches the controller malformed, and only the extended
//! advertising data commands do: `LeSetExtAdvData` and `LeSetExtScanResponseData`, both refused
//! with Invalid HCI Command Parameters (0x12) on D0:1C. The legacy data commands carry a fixed
//! array and are unaffected.
//!
//! [`SyncCommands`] encodes each command with the sync encoder into a buffer and writes it whole.
//! Every other packet, ACL data included, takes the connector's own path untouched. The wrapper
//! goes once `trouble-host` builds on a `bt-hci` whose async encoder writes the right length.

use bt_hci::transport::{PacketKind, PacketToController, PacketToHost, Transport};
use embedded_io_async::Write as AsyncWrite;

/// The longest command: its 3-byte header and 255 bytes of parameters.
const CMD_MAX: usize = 3 + 255;

/// A transport that is `T`, but writes commands through the sync encoder.
pub struct SyncCommands<T>(pub T);

impl<T: embedded_io::ErrorType> embedded_io::ErrorType for SyncCommands<T> {
    type Error = T::Error;
}

impl<T: Transport> Transport for SyncCommands<T> {
    async fn read<'a, P: PacketToHost<'a>>(&self, rx: &'a mut [u8]) -> Result<P, Self::Error> {
        self.0.read(rx).await
    }

    async fn write<P: PacketToController>(&self, tx: &P) -> Result<(), Self::Error> {
        self.0.write(&SyncEncoded(tx)).await
    }
}

/// A packet whose async encoding is its sync one, for a command that fits [`CMD_MAX`]. Any other
/// packet, or a command the sync encoder cannot write whole, keeps its own async encoding.
struct SyncEncoded<'a, P>(&'a P);

impl<P: PacketToController> PacketToController for SyncEncoded<'_, P> {
    const KIND: PacketKind = P::KIND;

    fn size(&self) -> usize {
        self.0.size()
    }

    fn write_hci<W: embedded_io::Write>(&self, writer: W) -> Result<(), W::Error> {
        self.0.write_hci(writer)
    }

    async fn write_hci_async<W: AsyncWrite>(&self, mut writer: W) -> Result<(), W::Error> {
        let size = self.0.size();
        if P::KIND == PacketKind::Cmd && size <= CMD_MAX {
            let mut buf = [0u8; CMD_MAX];
            let mut rest = &mut buf[..size];
            if self.0.write_hci(&mut rest).is_ok() && rest.is_empty() {
                return writer.write_all(&buf[..size]).await;
            }
        }
        self.0.write_hci_async(writer).await
    }
}
