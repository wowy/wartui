//! The node's app-only image, as an over-the-air update carries it: read, checked, and found
//! for each board.
//!
//! **The image says what it is.** Every node image carries an `esp_app_desc_t` at the start
//! of its first segment, holding the crate version and a project name per board
//! (`firmware/node/src/main.rs`, `PROJECT_NAME`). The update takes both from there rather than
//! from the file's name or a flag, so an image cannot be sent to a board it was not built for,
//! nor announced as a version it is not. A merged image opens with a bootloader, whose first
//! segment holds no app descriptor, so it is refused rather than written into an app slot.
//!
//! **The image is checked against itself before it is sent.** The ESP image format carries a
//! one-byte XOR checksum over every segment's data and, when the header's `hash_appended` flag
//! is set, a SHA-256 of everything before it. The bootloader checks both and refuses to boot an
//! image that fails either, falling back to the old slot. The update's own hash only proves the
//! node holds the file it was handed, so a corrupt file would otherwise cost a full transfer and
//! a reboot to find out.
//!
//! **Each board gets its own image**, from the sources `flash-fleet` uses: `--image`, a build
//! of the checkout (with that board's features, saved without `--merge`), or this binary's
//! release, checked against its `SHA256SUMS`.

use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use wartui_proto::air::{Board, Version};

use super::{
    CHIP_ID_AT, Chip, HEADER_LEN, IMAGE_MAGIC, NODE_APP, RELEASE_TAG, Source, fetch, read, source,
};

/// The largest image an app slot holds: `ota_0` and `ota_1` in `firmware/node/partitions.csv`.
pub const SLOT_LEN: usize = 0x1E_0000;

/// The app descriptor's first word.
const APP_DESC_MAGIC: u32 = 0xABCD_5432;

/// Where `esp_app_desc_t` keeps the version and project name, each a NUL-padded string of 32.
const DESC_VERSION_AT: usize = 16;
const DESC_PROJECT_AT: usize = 48;
const DESC_FIELD_LEN: usize = 32;

/// The length of a segment header: load address, then data length.
const SEGMENT_HEADER_LEN: usize = 8;

/// The seed of the image's segment checksum (`ESP_CHECKSUM_MAGIC`).
const CHECKSUM_SEED: u8 = 0xEF;

/// Where the header says how many segments follow, and whether a SHA-256 is appended.
const SEGMENT_COUNT_AT: usize = 1;
const HASH_APPENDED_AT: usize = 23;

/// The length of the appended SHA-256.
const DIGEST_LEN: usize = 32;

/// Where a partition table sits in a merged image, and its entries' first two bytes.
const PARTITION_TABLE_AT: usize = 0x8000;
const PARTITION_MAGIC: [u8; 2] = [0xAA, 0x50];

/// An app-only node image and what its descriptor says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppImage {
    /// The image, as an app slot holds it.
    pub bytes: Vec<u8>,
    /// Its SHA-256.
    pub sha256: [u8; 32],
    /// The node firmware version it carries.
    pub version: Version,
    /// The board it was built for.
    pub board: Board,
}

/// The descriptor's project name for each board.
const fn project_name(board: Board) -> Option<&'static str> {
    match board {
        Board::Esp32c5 => Some("wartui-node-esp32c5"),
        Board::Esp32c6 => Some("wartui-node-esp32c6"),
        Board::Esp32c6Xiao => Some("wartui-node-esp32c6-xiao"),
        // No node runs on a P4.
        Board::Esp32p4 | Board::Unknown(_) => None,
    }
}

/// The node firmware's features for each board, as a release builds them.
const fn features(board: Board) -> Option<&'static [&'static str]> {
    match board {
        Board::Esp32c5 => Some(&["esp32c5"]),
        Board::Esp32c6 => Some(&["esp32c6"]),
        Board::Esp32c6Xiao => Some(&["esp32c6", "xiao-external-antenna"]),
        Board::Esp32p4 | Board::Unknown(_) => None,
    }
}

const fn chip(board: Board) -> Option<Chip> {
    match board {
        Board::Esp32c5 => Some(Chip::Esp32c5),
        Board::Esp32c6 | Board::Esp32c6Xiao => Some(Chip::Esp32c6),
        Board::Esp32p4 | Board::Unknown(_) => None,
    }
}

/// A board as an operator reads it.
#[must_use]
pub fn board_name(board: Board) -> String {
    match board {
        Board::Esp32c5 => "esp32c5".to_owned(),
        Board::Esp32c6 => "esp32c6".to_owned(),
        Board::Esp32c6Xiao => "esp32c6-xiao".to_owned(),
        Board::Esp32p4 => "esp32p4".to_owned(),
        Board::Unknown(byte) => format!("unknown board {byte}"),
    }
}

/// Read an app-only image and its descriptor.
///
/// # Errors
/// When the bytes are not one: no ESP image header, a merged image, no app descriptor, a
/// project name no node board has, a version that is not `x.y.z`, a chip ID that is not the
/// board's, or more than an app slot holds.
pub fn parse(bytes: Vec<u8>) -> Result<AppImage> {
    let Descriptor { project, version } = descriptor(&bytes)?;
    let Some(board) = [Board::Esp32c5, Board::Esp32c6, Board::Esp32c6Xiao]
        .into_iter()
        .find(|board| project_name(*board) == Some(project.as_str()))
    else {
        bail!("built for project '{project}', which is no node board");
    };
    let Some(version) = parse_version(&version) else {
        bail!("its version '{version}' is not major.minor.patch");
    };
    let id = u16::from_le_bytes([bytes[CHIP_ID_AT], bytes[CHIP_ID_AT + 1]]);
    let want = chip(board).map(Chip::image_id);
    ensure!(want == Some(id), "its header names chip ID {id}, and {project} is not that chip");
    check_integrity(&bytes)?;
    ensure!(bytes.len() <= SLOT_LEN, "{} bytes, and an app slot holds {SLOT_LEN}", bytes.len());
    let sha256 = Sha256::digest(&bytes).into();
    Ok(AppImage { bytes, sha256, version, board })
}

/// What an app image's descriptor says, as text.
pub(super) struct Descriptor {
    /// The project name, which says which firmware and board the image is for.
    pub project: String,
    /// The crate version.
    pub version: String,
}

/// Read the app descriptor at the start of an app-only image's first segment.
///
/// # Errors
/// When the bytes hold no ESP image header, or are a merged image, or the first segment holds
/// no descriptor.
pub(super) fn descriptor(bytes: &[u8]) -> Result<Descriptor> {
    ensure!(bytes.len() >= HEADER_LEN + SEGMENT_HEADER_LEN, "too short to hold an image header");
    ensure!(bytes[0] == IMAGE_MAGIC, "not an ESP image: no header at the start");
    let desc_at = HEADER_LEN + SEGMENT_HEADER_LEN;
    let first_segment = le_u32(bytes, HEADER_LEN + 4) as usize;
    let desc = bytes.get(desc_at..desc_at + DESC_PROJECT_AT + DESC_FIELD_LEN);
    let Some(desc) = desc.filter(|desc| {
        le_u32(desc, 0) == APP_DESC_MAGIC && first_segment >= DESC_PROJECT_AT + DESC_FIELD_LEN
    }) else {
        if bytes.get(PARTITION_TABLE_AT..PARTITION_TABLE_AT + 2) == Some(&PARTITION_MAGIC) {
            bail!("a merged image (bootloader, partition table and app); use the app-only image");
        }
        bail!("no app descriptor at the start of the first segment");
    };
    Ok(Descriptor {
        project: text(&desc[DESC_PROJECT_AT..DESC_PROJECT_AT + DESC_FIELD_LEN]),
        version: text(&desc[DESC_VERSION_AT..DESC_VERSION_AT + DESC_FIELD_LEN]),
    })
}

/// Check the segment checksum and, when the header says one is appended, the SHA-256, as the
/// bootloader will. The checksum is the XOR of every segment's data, seeded with
/// [`CHECKSUM_SEED`], in the last byte of the padding that takes the segments to a 16-byte
/// boundary. The SHA-256 covers every byte before it, checksum included.
///
/// # Errors
/// When the image is truncated, or either check fails.
pub(super) fn check_integrity(bytes: &[u8]) -> Result<()> {
    ensure!(bytes.len() >= HEADER_LEN, "too short to hold an image header");
    let mut at = HEADER_LEN;
    let mut checksum = CHECKSUM_SEED;
    for segment in 0..bytes[SEGMENT_COUNT_AT] {
        let Some(header) = bytes.get(at..at + SEGMENT_HEADER_LEN) else {
            bail!("the image is truncated: segment {segment} has no header");
        };
        let len = le_u32(header, 4) as usize;
        let start = at + SEGMENT_HEADER_LEN;
        let Some(data) = bytes.get(start..start + len) else {
            bail!("the image is truncated: segment {segment} runs past its end");
        };
        checksum = data.iter().fold(checksum, |sum, byte| sum ^ byte);
        at = start + len;
    }
    let stored_at = at + 15 - at % 16;
    let Some(stored) = bytes.get(stored_at) else {
        bail!("the image is truncated: no checksum after its segments");
    };
    ensure!(
        *stored == checksum,
        "the image is corrupt: its checksum byte is {stored:#04x}, and its segments give \
         {checksum:#04x}"
    );
    if bytes[HASH_APPENDED_AT] == 1 {
        let end = stored_at + 1;
        let Some(digest) = bytes.get(end..end + DIGEST_LEN) else {
            bail!("the image is truncated: no SHA-256 after its checksum");
        };
        ensure!(
            Sha256::digest(&bytes[..end])[..] == *digest,
            "the image is corrupt: its appended SHA-256 does not match its contents"
        );
    }
    Ok(())
}

/// Read and parse the image at `path`.
///
/// # Errors
/// When it cannot be read, or [`parse`] refuses it.
pub fn load(path: &Path) -> Result<AppImage> {
    parse(read(path)?).with_context(|| format!("checking {}", path.display()))
}

/// Where `board`'s image comes from: a build of `firmware_dir` or the checkout, else this
/// binary's release.
///
/// # Errors
/// For a board no node firmware is built for.
pub fn source_for(board: Board, firmware_dir: Option<&Path>) -> Result<Source> {
    let Some(features) = features(board) else {
        bail!("no node firmware is built for {}", board_name(board));
    };
    source(&NODE_APP, None, firmware_dir, features, false, RELEASE_TAG)
}

/// Build or fetch `board`'s image from `source`, and check it is that board's.
///
/// # Errors
/// When the build or download fails, or the image is not `board`'s.
pub fn obtain(board: Board, source: &Source) -> Result<AppImage> {
    let (Some(features), Some(chip)) = (features(board), chip(board)) else {
        bail!("no node firmware is built for {}", board_name(board));
    };
    let image = load(&fetch(&NODE_APP, source, features, false, chip)?)?;
    ensure!(
        image.board == board,
        "the image built for {} says it is for {}",
        board_name(board),
        board_name(image.board)
    );
    Ok(image)
}

fn le_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// A NUL-padded field as text.
fn text(field: &[u8]) -> String {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// `0.1.1`, each part a byte.
fn parse_version(text: &str) -> Option<Version> {
    let mut parts = text.split('.').map(|part| part.parse::<u8>().ok());
    let (Some(Some(major)), Some(Some(minor)), Some(Some(patch)), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    Some(Version::new(major, minor, patch))
}

/// A hand-built app-only image of `len` bytes, a multiple of 16, for tests: one segment holding
/// the descriptor, then the checksum and the SHA-256.
#[cfg(test)]
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
#[cfg(test)]
pub(super) fn fixture_with(
    header: [u8; HEADER_LEN],
    project: &str,
    version: &str,
    len: usize,
) -> Vec<u8> {
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
#[cfg(test)]
fn seal(image: &mut Vec<u8>) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_image_reads_board_and_version_when_descriptor_is_present() {
        for board in [Board::Esp32c5, Board::Esp32c6, Board::Esp32c6Xiao] {
            let image = parse(fixture(board, "0.1.1", 0x1000)).unwrap();
            assert_eq!((image.board, image.version), (board, Version::new(0, 1, 1)));
            assert_eq!(image.sha256, <[u8; 32]>::from(Sha256::digest(&image.bytes)));
        }
    }

    #[test]
    fn app_image_reads_espflash_layout_when_given_real_header() {
        // The first 0x35 bytes `espflash save-image` wrote for a C5 node, without `--merge`,
        // cut to one short segment and sealed again.
        let mut bytes = vec![0u8; 0x400];
        let head: [u8; 0x34] = [
            0xe9, 0x05, 0x02, 0x20, 0xfe, 0x77, 0x01, 0x42, 0xee, 0x00, 0x00, 0x00, 0x17, 0x00,
            0x00, 0x00, 0x00, 0xc7, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x20, 0x00, 0x00, 0x42,
            0x68, 0x77, 0x01, 0x00, 0x32, 0x54, 0xcd, 0xab, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, b'0', b'.', b'1', b'.',
        ];
        bytes[..head.len()].copy_from_slice(&head);
        bytes[0x34] = b'1';
        bytes[0x50..0x63].copy_from_slice(b"wartui-node-esp32c5");
        bytes[SEGMENT_COUNT_AT] = 1;
        bytes[HEADER_LEN + 4..HEADER_LEN + 8].copy_from_slice(&0x3D0u32.to_le_bytes());
        seal(&mut bytes);
        let image = parse(bytes).unwrap();
        assert_eq!((image.board, image.version), (Board::Esp32c5, Version::new(0, 1, 1)));
    }

    #[test]
    fn app_image_refuses_merged_image_when_bootloader_leads() {
        // A bootloader's first segment holds no app descriptor; the partition table at
        // 0x8000 names what it is.
        let mut merged = vec![0xFFu8; 0x9000];
        merged[..24].copy_from_slice(&[0xe9; 24]);
        merged[HEADER_LEN + 4..HEADER_LEN + 8].copy_from_slice(&0x100u32.to_le_bytes());
        merged[PARTITION_TABLE_AT..PARTITION_TABLE_AT + 2].copy_from_slice(&PARTITION_MAGIC);
        let error = parse(merged).unwrap_err().to_string();
        assert!(error.contains("merged image"), "{error}");
    }

    #[test]
    fn app_image_refuses_image_when_descriptor_is_missing() {
        let mut bytes = fixture(Board::Esp32c6, "0.1.1", 0x1000);
        bytes[HEADER_LEN + SEGMENT_HEADER_LEN] ^= 0xFF;
        assert!(parse(bytes).unwrap_err().to_string().contains("no app descriptor"));
    }

    #[test]
    fn app_image_refuses_image_when_project_is_no_node_board() {
        let bytes = fixture(Board::Unknown(9), "0.1.1", 0x1000);
        let error = parse(bytes).unwrap_err().to_string();
        assert!(error.contains("'not-a-node'"), "{error}");
    }

    #[test]
    fn app_image_refuses_image_when_version_is_not_three_numbers() {
        for version in ["0.1", "0.1.1.1", "0.1.x", "0.1.256", ""] {
            let bytes = fixture(Board::Esp32c5, version, 0x1000);
            assert!(parse(bytes).is_err(), "{version:?} was accepted");
        }
    }

    #[test]
    fn app_image_refuses_image_when_chip_id_is_not_the_boards() {
        let mut bytes = fixture(Board::Esp32c5, "0.1.1", 0x1000);
        bytes[CHIP_ID_AT] = 13;
        assert!(parse(bytes).unwrap_err().to_string().contains("chip ID 13"));
    }

    #[test]
    fn app_image_refuses_image_when_larger_than_a_slot() {
        assert!(parse(fixture(Board::Esp32c5, "0.1.1", SLOT_LEN)).is_ok());
        let error = parse(fixture(Board::Esp32c5, "0.1.1", SLOT_LEN + 16)).unwrap_err();
        assert!(error.to_string().contains("an app slot holds"), "{error}");
    }

    #[test]
    fn app_image_refuses_file_when_too_short_or_not_an_image() {
        assert!(parse(vec![0xe9; 16]).is_err());
        let mut bytes = fixture(Board::Esp32c5, "0.1.1", 0x1000);
        bytes[0] = 0x7F;
        assert!(parse(bytes).is_err());
    }

    #[test]
    fn app_image_names_release_asset_when_board_is_known() {
        for (board, asset) in [
            (Board::Esp32c5, "wartui-node-app-esp32c5.bin"),
            (Board::Esp32c6, "wartui-node-app-esp32c6.bin"),
            (Board::Esp32c6Xiao, "wartui-node-app-esp32c6-xiao.bin"),
        ] {
            let found = source(&NODE_APP, None, None, features(board).unwrap(), false, Some("v1"));
            let expected = Source::Release { tag: "v1".to_owned(), asset };
            assert_eq!(found.unwrap(), expected);
        }
        assert!(source_for(Board::Unknown(7), None).is_err());
    }

    #[test]
    fn app_image_refuses_image_when_segment_byte_is_flipped() {
        let mut bytes = fixture(Board::Esp32c6, "0.1.1", 0x1000);
        bytes[0x800] ^= 0x01;
        let error = parse(bytes).unwrap_err().to_string();
        assert!(error.contains("corrupt") && error.contains("checksum"), "{error}");
    }

    #[test]
    fn app_image_refuses_image_when_appended_hash_byte_is_flipped() {
        let mut bytes = fixture(Board::Esp32c6, "0.1.1", 0x1000);
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        let error = parse(bytes).unwrap_err().to_string();
        assert!(error.contains("SHA-256"), "{error}");
    }

    #[test]
    fn app_image_refuses_image_when_truncated() {
        let bytes = fixture(Board::Esp32c6, "0.1.1", 0x1000);
        let error = parse(bytes[..0x800].to_vec()).unwrap_err().to_string();
        assert!(error.contains("truncated"), "{error}");
    }
}
