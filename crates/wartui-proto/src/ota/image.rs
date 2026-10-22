//! The SHA-256 digest espflash appends to an ESP app image, which names the build.
//!
//! The P4 bridge compares the digest its C6 runs with the digest of the image it bundles. Each
//! is read from the image itself, so one parser serves the C6, reading its booted slot from
//! flash, and the P4's build script, reading the bundled `.bin`. The digest needs no version
//! string kept in step, and two builds that differ in any byte differ in it.
//!
//! The layout, little-endian:
//!
//! - a 24-byte header: magic `0xE9` at 0, the segment count at 1, `hash_appended` at 23;
//! - each segment: an 8-byte header (load address, then data length) and its data;
//! - zero padding that puts a checksum byte at the end of a 16-byte block;
//! - with `hash_appended` 1, the digest: SHA-256 of every byte before it, which the
//!   bootloader checks at boot.

/// Bytes in a SHA-256 digest: the appended one, and the whole-file one an update checks.
pub const DIGEST_LEN: usize = 32;

const MAGIC: u8 = 0xE9;
const HEADER_LEN: u32 = 24;
const SEGMENT_HEADER_LEN: u32 = 8;
/// The most segments the bootloader loads.
const MAX_SEGMENTS: u8 = 16;

/// The digest appended to the image that `read` reads, or `None` when it has none or its
/// header is malformed.
///
/// `read(offset, buf)` fills `buf` from the image at `offset` and returns `false` when it
/// cannot, such as past the image's end.
pub fn appended_digest(mut read: impl FnMut(u32, &mut [u8]) -> bool) -> Option<[u8; DIGEST_LEN]> {
    let mut header = [0u8; HEADER_LEN as usize];
    if !read(0, &mut header) || header[0] != MAGIC || header[23] != 1 {
        return None;
    }
    let segments = header[1];
    if segments == 0 || segments > MAX_SEGMENTS {
        return None;
    }
    let mut at = HEADER_LEN;
    for _ in 0..segments {
        let mut segment = [0u8; SEGMENT_HEADER_LEN as usize];
        if !read(at, &mut segment) {
            return None;
        }
        let len = u32::from_le_bytes([segment[4], segment[5], segment[6], segment[7]]);
        at = at.checked_add(SEGMENT_HEADER_LEN)?.checked_add(len)?;
    }
    // The checksum byte ends a 16-byte block, and the digest follows it.
    let digest_at = at.checked_add(1)?.checked_next_multiple_of(16)?;
    let mut digest = [0u8; DIGEST_LEN];
    read(digest_at, &mut digest).then_some(digest)
}

/// The digest appended to `image`, held whole in memory, as [`appended_digest`] reads it.
#[must_use]
pub fn appended_digest_of(image: &[u8]) -> Option<[u8; DIGEST_LEN]> {
    appended_digest(|at, buf| {
        let bytes = usize::try_from(at)
            .ok()
            .and_then(|start| image.get(start..start.checked_add(buf.len())?));
        bytes.map(|bytes| buf.copy_from_slice(bytes)).is_some()
    })
}
