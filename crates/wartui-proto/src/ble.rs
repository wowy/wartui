//! The Bluetooth host link's GATT identity, and the C6's buffer of what it has yet to notify.
//!
//! The host link is the GATT service [`SERVICE_UUID`]. [`UP_UUID`] notifies the host stream a USB
//! host reads, split across notifications anywhere, and [`DOWN_UUID`] takes host frames, framed as
//! on USB, in writes without response that end in a write request. The identity lives here so the
//! C6 and every host read the same one; a change strands whichever end was not rebuilt. Beside it
//! the C6 offers a phone the WardriveESP service ([`crate::wardrive_esp::frame`]).
//!
//! The C6 advertises [`NAME_PREFIX`] and the bridge's last two octets (`wartui 96:C4`), the form
//! `--bridge` takes, so an operator picks the bridge by the same name over either link.
//!
//! **The C6 serves an ATT MTU of [`ATT_MTU`]**, so no write to Down carries more than
//! [`DOWN_VALUE_MAX`] value bytes, and a central's own stack keeps to it. wartui's transport caps
//! its pieces at [`DOWN_VALUE_MAX`] whatever the MTU, so it also updates a bridge whose C6 serves
//! a larger one.
//!
//! The C6 holds the stream it has yet to notify in a [`ByteRing`] of
//! [`HOST_WINDOW_BYTES`](crate::radio_link::HOST_WINDOW_BYTES), which the P4's credit window
//! never overfills.

/// The "wartui link" GATT service.
pub const SERVICE_UUID: u128 = 0xf241_0001_d870_4eb6_a5a0_ed64_5732_a877;
/// The service's Up characteristic, notify: the bridge's frame stream.
pub const UP_UUID: u128 = 0xf241_0002_d870_4eb6_a5a0_ed64_5732_a877;
/// The service's Down characteristic, written with or without response: the host's frames.
pub const DOWN_UUID: u128 = 0xf241_0003_d870_4eb6_a5a0_ed64_5732_a877;
/// What the C6 advertises ahead of the bridge's last two octets: `wartui 96:C4`.
pub const NAME_PREFIX: &str = "wartui ";

/// The ATT MTU the C6 serves.
///
/// A conforming central sends the C6 no ATT PDU over 182 bytes, so no L2CAP PDU over 186, and no
/// ACL packet, nor any fragment its controller cuts from one, over 190 with its 4-byte header.
/// The C6's controller hands over a packet of 193 to 198 bytes as a chain of two buffers, 190
/// bytes in the first, and esp-radio 1.0.0-beta.1 copies only that first buffer. The short packet
/// stops the C6's Bluetooth host (`docs/ble-host-link-findings.md`, "The host stop, found").
pub const ATT_MTU: u16 = 182;

/// The most value bytes one write to Down carries: [`ATT_MTU`] less the ATT opcode and handle.
///
/// A central on the C6's MTU cannot write more. wartui's transport caps its pieces at it whatever
/// the MTU, so it also updates a bridge whose C6 serves a larger one.
pub const DOWN_VALUE_MAX: usize = ATT_MTU as usize - 3;

const _: () = assert!(
    ATT_MTU as usize + 8 <= 190,
    "an ATT PDU of ATT_MTU bytes makes an ACL packet the C6's controller chains"
);

/// The host stream bytes the C6 holds until it notifies them: a byte ring, oldest first.
#[derive(Debug)]
pub struct ByteRing<const N: usize> {
    buf: [u8; N],
    /// Where the oldest byte is.
    head: usize,
    len: usize,
}

impl<const N: usize> ByteRing<N> {
    /// Empty. Const, so firmware builds it in a static.
    #[must_use]
    pub const fn new() -> Self {
        Self { buf: [0; N], head: 0, len: 0 }
    }

    /// Appends as much of `bytes` as fits and returns how much did.
    pub fn push(&mut self, bytes: &[u8]) -> usize {
        let n = bytes.len().min(N - self.len);
        let tail = (self.head + self.len) % N;
        let first = n.min(N - tail);
        self.buf[tail..tail + first].copy_from_slice(&bytes[..first]);
        self.buf[..n - first].copy_from_slice(&bytes[first..n]);
        self.len += n;
        n
    }

    /// Moves the oldest bytes into `out`, as many as it holds, and returns how many.
    pub fn take(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.len);
        let first = n.min(N - self.head);
        out[..first].copy_from_slice(&self.buf[self.head..self.head + first]);
        out[first..n].copy_from_slice(&self.buf[..n - first]);
        self.head = (self.head + n) % N;
        self.len -= n;
        n
    }

    /// Moves the oldest bytes into `out` up to and including the first `delim`, and returns how
    /// many. Stops short of it at `out.len()` bytes or at all it holds.
    pub fn take_through(&mut self, out: &mut [u8], delim: u8) -> usize {
        let limit = out.len().min(self.len);
        let n =
            (0..limit).find(|&i| self.buf[(self.head + i) % N] == delim).map_or(limit, |i| i + 1);
        self.take(&mut out[..n])
    }

    /// Discards everything held.
    pub const fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// Bytes held.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing is held.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<const N: usize> Default for ByteRing<N> {
    fn default() -> Self {
        Self::new()
    }
}
