//! Fixtures for update tests, here and in a front end: hand-built app-only images that pass every
//! check [`super::image`] makes, and image headers as espflash writes them.

use sha2::{Digest, Sha256};
use wartui_proto::bus::Board;

use super::image::{
    APP_DESC_MAGIC, CHECKSUM_SEED, CHIP_ID_AT, Chip, DESC_PROJECT_AT, DESC_VERSION_AT,
    HASH_APPENDED_AT, HEADER_LEN, IMAGE_MAGIC, SEGMENT_COUNT_AT, SEGMENT_HEADER_LEN, chip, le_u32,
    project_name,
};

/// The first 24 bytes `espflash save-image --merge` wrote for a C6, from one ELF.
pub const C6_HEADER: [u8; 24] = [
    0xe9, 0x03, 0x02, 0x20, 0x1a, 0xb9, 0x86, 0x40, 0xee, 0x00, 0x00, 0x00, 0x0d, 0x00, 0x00, 0x00,
    0x00, 0x63, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
];

/// The first 24 bytes `espflash save-image --merge` wrote for a C5, from the same ELF.
pub const C5_HEADER: [u8; 24] = [
    0xe9, 0x03, 0x02, 0x20, 0xaa, 0xbb, 0x84, 0x40, 0xee, 0x00, 0x00, 0x00, 0x17, 0x00, 0x00, 0x64,
    0x00, 0xc7, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
];

/// The first 24 bytes `espflash save-image` 4.6 wrote for the P4 bridge's app-only image.
pub const P4_APP_HEADER: [u8; 24] = [
    0xe9, 0x05, 0x02, 0x20, 0x76, 0x8a, 0x08, 0x40, 0xee, 0x00, 0x00, 0x00, 0x12, 0x00, 0x00, 0x2c,
    0x01, 0x8f, 0x01, 0x00, 0x00, 0x00, 0x00, 0x01,
];

/// A hand-built app-only node image of `len` bytes, a multiple of 16: one segment holding the
/// descriptor, then the checksum and the SHA-256. A board no node runs on gets chip ID 0 and the
/// project `not-a-node`.
#[must_use]
pub fn fixture(board: Board, version: &str, len: usize) -> Vec<u8> {
    let mut header = [0u8; HEADER_LEN];
    header[0] = IMAGE_MAGIC;
    let id = chip(board).map_or(0, Chip::image_id);
    header[CHIP_ID_AT..CHIP_ID_AT + 2].copy_from_slice(&id.to_le_bytes());
    let project = project_name(board).unwrap_or("not-a-node");
    fixture_with(header, project, version, len)
}

/// A hand-built app-only image as [`fixture`] makes one, behind `header` and naming `project`.
/// The header is set to one segment and an appended SHA-256.
#[must_use]
pub fn fixture_with(header: [u8; HEADER_LEN], project: &str, version: &str, len: usize) -> Vec<u8> {
    let mut image = header.to_vec();
    image[SEGMENT_COUNT_AT] = 1;
    image[HASH_APPENDED_AT] = 1;
    // Header, segment header, checksum padding and digest take 80 bytes.
    let segment = len.max(0x200) - 80;
    image.extend_from_slice(&0x4200_0020u32.to_le_bytes());
    image.extend_from_slice(&u32::try_from(segment).unwrap_or(0).to_le_bytes());
    let desc = image.len();
    image.resize(desc + segment, 0x5A);
    image[desc..desc + 0x100].fill(0);
    image[desc..desc + 4].copy_from_slice(&APP_DESC_MAGIC.to_le_bytes());
    let put = |image: &mut Vec<u8>, at: usize, value: &str| {
        image[desc + at..desc + at + value.len()].copy_from_slice(value.as_bytes());
    };
    put(&mut image, DESC_VERSION_AT, version);
    put(&mut image, DESC_PROJECT_AT, project);
    seal(&mut image);
    image
}

/// Append the checksum padding, the checksum and the SHA-256 to a header and its segments.
pub fn seal(image: &mut Vec<u8>) {
    let mut at = HEADER_LEN;
    let mut checksum = CHECKSUM_SEED;
    for _ in 0..image[SEGMENT_COUNT_AT] {
        let len = le_u32(image, at + 4) as usize;
        let start = at + SEGMENT_HEADER_LEN;
        checksum = image[start..start + len].iter().fold(checksum, |sum, byte| sum ^ byte);
        at = start + len;
    }
    image.truncate(at);
    image.resize(at + 15 - at % 16, 0);
    image.push(checksum);
    let digest = Sha256::digest(&image[..]);
    image.extend_from_slice(&digest);
}
