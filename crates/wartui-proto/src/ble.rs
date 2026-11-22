//! The Bluetooth host link's GATT identity, the phones' rest, and the C6's buffer of what it has
//! yet to notify.
//!
//! The host link is the GATT service [`SERVICE_UUID`]. [`UP_UUID`] notifies the host stream a USB
//! host reads, split across notifications anywhere, and [`DOWN_UUID`] takes host frames, framed as
//! on USB, in writes without response that end in a write request. The identity lives here so the
//! C6 and every host read the same one; a change strands whichever end was not rebuilt. Beside it
//! the C6 offers a phone the WardriveESP service ([`crate::wardrive_esp::frame`]).
//!
//! **A host finds a bridge by its id, not its name.** The id is service data under [`ID_UUID`]
//! carrying the bridge's last two octets, the form `--bridge` takes ([`advertised_octets`]). The
//! name, [`NAME_PREFIX`] and the same octets (`wartui 96:C4`), is a label a person reads.
//!
//! **The id is in the wartui pair alone.** That pair advertises from [`link_address`], apart from
//! the phones' pair and the status beacon on the public address, because a phone that has linked
//! reconnects by address and would otherwise land on the wartui pair.
//!
//! **The phones' set rests after a silent central** ([`Rest`]). A phone that has linked connects
//! on the phones' set with its app closed, says nothing, and is back as soon as it is dropped. So
//! the C6 drops it and stops advertising the phones' set for a while, and the wartui set stays
//! reachable meanwhile. Each silent central in a row doubles the rest, up to [`REST_MAX_MS`]: the
//! longest an opened app waits to link. The run ends when none is dropped for [`REST_MAX_MS`]
//! after a rest.
//!
//! **The C6 serves an ATT MTU of [`ATT_MTU`]**, so no write to Down carries more than
//! [`DOWN_VALUE_MAX`] value bytes, and a central's own stack keeps to it. wartui's transport caps
//! its pieces at [`DOWN_VALUE_MAX`] whatever the MTU, so it also updates a bridge whose C6 serves
//! a larger one.
//!
//! The C6 holds the stream it has yet to notify in a [`ByteRing`] of
//! [`HOST_WINDOW_BYTES`](crate::radio_link::HOST_WINDOW_BYTES), which the P4's credit window
//! never overfills.

use crate::mac::Mac;

/// The "wartui link" GATT service.
pub const SERVICE_UUID: u128 = 0xf241_0001_d870_4eb6_a5a0_ed64_5732_a877;
/// The service's Up characteristic, notify: the bridge's frame stream.
pub const UP_UUID: u128 = 0xf241_0002_d870_4eb6_a5a0_ed64_5732_a877;
/// The service's Down characteristic, written with or without response: the host's frames.
pub const DOWN_UUID: u128 = 0xf241_0003_d870_4eb6_a5a0_ed64_5732_a877;
/// What the C6 names itself ahead of the bridge's last two octets: `wartui 96:C4`.
pub const NAME_PREFIX: &str = "wartui ";
/// The 16-bit UUID of the service data that marks a bridge and carries its last two octets.
pub const ID_UUID: u16 = 0x5754;

/// The bridge's last two octets from one advertisement's service data under [`ID_UUID`], or
/// `None` for a device that is not a bridge: an id that is absent or not two bytes.
#[must_use]
pub fn advertised_octets(id: Option<&[u8]>) -> Option<[u8; 2]> {
    match id? {
        &[first, second] => Some([first, second]),
        _ => None,
    }
}

/// The address the wartui pair advertises from: a static random address made from the bridge's
/// MAC, so it is the same on every boot and a host's bond stays good. Display order, as [`Mac`].
#[must_use]
pub const fn link_address(mac: &Mac) -> Mac {
    let mut address = *mac;
    // The top two bits of the first octet mark a static random address.
    address[0] |= 0xC0;
    address
}

/// The first rest, in ms.
pub const REST_FIRST_MS: u64 = 5_000;
/// The longest rest, in ms: how long an opened app may wait to link.
pub const REST_MAX_MS: u64 = 30_000;

/// When the phones' set rests: [`REST_FIRST_MS`] after the first silent central, doubled for each
/// one in a row after it, up to [`REST_MAX_MS`]. A silent central is in the run when it is dropped
/// no more than [`REST_MAX_MS`] after the last rest ends; a phone that keeps knocking is dropped a
/// few seconds after each. Times are the caller's uptime, in ms.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rest {
    /// Silent centrals dropped in a row.
    kicks: u8,
    /// When the last rest ends, or `None` before one.
    ends: Option<u64>,
}

impl Rest {
    /// No rest has run.
    #[must_use]
    pub const fn new() -> Self {
        Self { kicks: 0, ends: None }
    }

    /// A silent central was dropped at `now_ms`: start the next rest and return its length in ms.
    /// More than [`REST_MAX_MS`] past the last rest's end, the run is over and this rest is the
    /// first.
    pub fn begin(&mut self, now_ms: u64) -> u64 {
        if self.ends.is_some_and(|ends| now_ms.saturating_sub(ends) > REST_MAX_MS) {
            self.kicks = 0;
        }
        // Past a shift of 3 the length is the cap; 8 keeps the product far inside a `u64`.
        let length = (REST_FIRST_MS << self.kicks.min(8)).min(REST_MAX_MS);
        self.kicks = self.kicks.saturating_add(1);
        self.ends = Some(now_ms.saturating_add(length));
        length
    }

    /// When the rest ends, while one runs at `now_ms`.
    #[must_use]
    pub fn until(&self, now_ms: u64) -> Option<u64> {
        self.ends.filter(|&ends| now_ms < ends)
    }

    /// A phone reads its stream: the next rest is the first again, and none runs.
    pub const fn clear(&mut self) {
        *self = Self::new();
    }
}

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
