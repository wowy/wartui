//! The P4's node image cache, as the link names it.
//!
//! The host uploads a node image once ([`HostToBridge::CacheImage`]), then asks for each chunk
//! by its place in it ([`HostToBridge::ToNodeChunk`]), so the image crosses the host link once
//! however many nodes take it. The P4 builds each [`UpdateChunk`] record from its copy with
//! [`cache_chunk_body`].
//!
//! [`NodeImageCache`] holds the cache's rules, so the P4 (`firmware/p4-bridge/src/cache.rs`) and
//! the simulator keep the same cache and build the same bytes. The P4's module docs give the
//! reasoning behind them.
//!
//! [`HostToBridge::CacheImage`]: super::HostToBridge::CacheImage
//! [`HostToBridge::ToNodeChunk`]: super::HostToBridge::ToNodeChunk

use core::borrow::BorrowMut;

use crate::bus::{UPDATE_CHUNK, UPDATE_CHUNK_MAX, UpdateChunk, UpdateState, UpdateStatus};
use crate::ota::image::DIGEST_LEN;
use crate::ota::{Receiver, SECTOR, SectorSink};
use crate::radio_link::update::{Op, Received, receive};

/// The tag a [`super::HostToBridge::ToNodeChunk`] names a cached image by: the first four bytes
/// of its SHA-256, little-endian.
///
/// A chunk command naming another tag than the cached image's is refused, so a session cannot
/// send from an image that replaced its own.
#[must_use]
pub fn image_tag(sha256: &[u8; DIGEST_LEN]) -> u32 {
    u32::from_le_bytes([sha256[0], sha256[1], sha256[2], sha256[3]])
}

/// Encode into `out` the [`UpdateChunk`] body for `len` bytes of `image` at `offset`, returning
/// its length.
///
/// `None` when `len` is 0 or over [`UPDATE_CHUNK`], or the range runs past `image`'s end.
/// Nothing is written then.
#[must_use]
pub fn cache_chunk_body(
    image: &[u8],
    session: u8,
    offset: u32,
    len: u16,
    out: &mut [u8; UPDATE_CHUNK_MAX],
) -> Option<usize> {
    let len = usize::from(len);
    if len == 0 || len > UPDATE_CHUNK {
        return None;
    }
    let start = usize::try_from(offset).ok()?;
    let data = image.get(start..start.checked_add(len)?)?;
    UpdateChunk { session, offset, data }.encode_into(out)
}

/// A node image cache: a [`Receiver`] filling `region` from `CacheImage` ops, and the image it
/// serves chunks from once verified.
///
/// The receiver's board is a node's, filled into each `Begin`, so the check passes every image.
/// An empty `region` is a bridge with no memory for a cache: every `Begin` is refused with
/// [`UpdateState::FlashError`], and no image is served.
#[derive(Debug)]
pub struct NodeImageCache<R, M> {
    receiver: R,
    region: M,
    /// The [`image_tag`] of the receiver's session, from the `Begin` it accepted.
    tag: Option<u32>,
}

impl<R: BorrowMut<Receiver>, M: BorrowMut<[u8]>> NodeImageCache<R, M> {
    /// A cache with no image, over `region`.
    #[must_use]
    pub const fn new(receiver: R, region: M) -> Self {
        Self { receiver, region, tag: None }
    }

    /// Bytes the region holds.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.region.borrow().len()
    }

    /// Hand `op` to the receiver, returning the status to send.
    ///
    /// An accepted `Begin` makes its image the one the cache holds. A refused one leaves the
    /// session, and so its tag, as they were. `Install` is refused with
    /// [`UpdateState::FlashError`]: a cached image only feeds chunks.
    pub fn take(&mut self, op: Op<'_>) -> Option<UpdateStatus> {
        let begin = match op {
            Op::Begin { session, sha256, .. } => Some((session, image_tag(&sha256))),
            _ => None,
        };
        let mut sink = Region(self.region.borrow_mut());
        let slot = (!sink.0.is_empty()).then_some(&mut sink);
        match receive(self.receiver.borrow_mut(), op, slot, false) {
            Received::Status(status) => {
                if let (Some((session, tag)), Some(status)) = (begin, status)
                    && status.session == session
                    && matches!(status.state, UpdateState::Receiving | UpdateState::Verified)
                {
                    self.tag = Some(tag);
                }
                status
            }
            Received::Install(session) => {
                Some(UpdateStatus { session, state: UpdateState::FlashError, next_offset: 0 })
            }
        }
    }

    /// The verified image tagged `tag`, if the cache holds it.
    #[must_use]
    pub fn image(&self, tag: u32) -> Option<&[u8]> {
        let receiver = self.receiver.borrow();
        if !receiver.is_verified() || self.tag != Some(tag) {
            return None;
        }
        let len = usize::try_from(receiver.total_len()?).ok()?;
        self.region.borrow().get(..len)
    }

    /// Drop the session and the image it filled.
    pub fn reset(&mut self) {
        self.receiver.borrow_mut().reset();
        self.tag = None;
    }
}

/// The cache's region as a receiver's slot.
struct Region<'a>(&'a mut [u8]);

/// A write or erase past the region's end, which the receiver's length check rules out.
struct OutOfRange;

impl SectorSink for Region<'_> {
    type Error = OutOfRange;

    fn capacity(&self) -> u32 {
        u32::try_from(self.0.len()).unwrap_or(u32::MAX)
    }

    /// Memory needs no erase.
    fn erase_sector(&mut self, offset: u32) -> Result<(), OutOfRange> {
        self.range(offset, SECTOR).map(|_| ())
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), OutOfRange> {
        self.range(offset, data.len())?.copy_from_slice(data);
        Ok(())
    }
}

impl Region<'_> {
    fn range(&mut self, offset: u32, len: usize) -> Result<&mut [u8], OutOfRange> {
        let start = usize::try_from(offset).map_err(|_| OutOfRange)?;
        let end = start.checked_add(len).ok_or(OutOfRange)?;
        self.0.get_mut(start..end).ok_or(OutOfRange)
    }
}
