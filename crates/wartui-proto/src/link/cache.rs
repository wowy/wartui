//! The P4's node image cache, as the link names it.
//!
//! The host uploads a node image once ([`HostToBridge::CacheImage`]), then asks for each chunk
//! by its place in it ([`HostToBridge::ToNodeChunk`]), so the image crosses the host link once
//! however many nodes take it. The P4 builds each [`UpdateChunk`] record from its copy with
//! [`cache_chunk_body`], which the simulator calls too, so both build the same bytes.
//!
//! [`HostToBridge::CacheImage`]: super::HostToBridge::CacheImage
//! [`HostToBridge::ToNodeChunk`]: super::HostToBridge::ToNodeChunk

use crate::bus::{UPDATE_CHUNK, UPDATE_CHUNK_MAX, UpdateChunk};
use crate::ota::image::DIGEST_LEN;

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
