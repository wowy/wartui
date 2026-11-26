//! The app-only images an update carries, a node's and the P4 bridge's, and their checks.
//!
//! **The image says what it is.** Every node image carries an `esp_app_desc_t` at the start
//! of its first segment, holding the crate version and the board's project name
//! (`firmware/node/src/main.rs`, `PROJECT_NAME`). The update takes both from there rather than
//! from the file's name or a flag, so an image cannot be sent to a board it was not built for,
//! nor announced as a version it is not. A merged image opens with a bootloader, whose first
//! segment holds no app descriptor, so it is refused rather than written into an app slot.
//!
//! **The image is checked against itself before it is sent.** The ESP image format carries a
//! one-byte XOR checksum over every segment's data and, when the header's `hash_appended` flag
//! is set, a SHA-256 of everything before it. The bootloader checks both and refuses to boot an
//! image that fails either, falling back to the old slot. The update's own hash only proves the
//! receiver holds the file it was handed, so a corrupt file would otherwise cost a full transfer
//! and a reboot to find out.
//!
//! **The P4 bridge's image is checked the same way, and more**: the P4's chip, an app-only
//! layout, the descriptor's project name ([`BRIDGE_PROJECT`]), the segment checksum and appended
//! SHA-256, the size of the P4's app slot ([`BRIDGE_SLOT_LEN`]), and the appended digest, which
//! names the image to the P4. The P4's receiver checks only that the bytes arrived whole, so a
//! wrong or damaged file would otherwise cost a transfer, a reboot and a rollback.
//!
//! **A checked image is the only kind there is.** [`NodeImage`] and [`BridgeImage`] keep their
//! fields private, so the only way to hold one is its `parse`, and no unchecked image reaches a
//! node or a bridge. Each check runs in a fixed order, so the same file is refused for the same
//! reason every time.

use std::sync::Arc;

use sha2::{Digest, Sha256};
use wartui_core::ota::OtaImage;
use wartui_proto::bus::{Board, Version};
use wartui_proto::ota::image::{DIGEST_LEN, appended_digest_of};

/// An ESP image header's first byte.
pub const IMAGE_MAGIC: u8 = 0xE9;

/// Where `esp_image_header_t` keeps the chip ID: after the 8-byte common header and the four
/// SPI pin and drive bytes of the extended one, as a little-endian `u16`.
pub const CHIP_ID_AT: usize = 12;

/// The whole header, common and extended, which a file must hold to be read at all.
pub const HEADER_LEN: usize = 24;

/// The largest image a node's app slot holds: `ota_0` and `ota_1` in
/// `firmware/node/partitions.csv`.
pub const SLOT_LEN: usize = 0x1E_0000;

/// The largest image the P4's app slot holds: `ota_0` and `ota_1` in
/// `firmware/p4-bridge/partitions.csv`.
pub const BRIDGE_SLOT_LEN: usize = 0x1E_0000;

/// The project name the P4 bridge's app descriptor carries: its crate's name.
pub const BRIDGE_PROJECT: &str = "wartui-p4-bridge-fw";

/// The app descriptor's first word.
pub(super) const APP_DESC_MAGIC: u32 = 0xABCD_5432;

/// Where `esp_app_desc_t` keeps the version and project name, each a NUL-padded string of 32.
pub(super) const DESC_VERSION_AT: usize = 16;
pub(super) const DESC_PROJECT_AT: usize = 48;
const DESC_FIELD_LEN: usize = 32;

/// The length of a segment header: load address, then data length.
pub(super) const SEGMENT_HEADER_LEN: usize = 8;

/// The seed of the image's segment checksum (`ESP_CHECKSUM_MAGIC`).
pub(super) const CHECKSUM_SEED: u8 = 0xEF;

/// Where the header says how many segments follow, and whether a SHA-256 is appended.
pub(super) const SEGMENT_COUNT_AT: usize = 1;
pub(super) const HASH_APPENDED_AT: usize = 23;

/// Where a partition table sits in a merged image, and its entries' first two bytes.
pub(super) const PARTITION_TABLE_AT: usize = 0x8000;
pub(super) const PARTITION_MAGIC: [u8; 2] = [0xAA, 0x50];

/// The chips the firmwares are built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chip {
    Esp32c5,
    Esp32c6,
    Esp32p4,
}

impl Chip {
    /// Every chip an image header here may name.
    pub const ALL: [Self; 3] = [Self::Esp32c5, Self::Esp32c6, Self::Esp32p4];

    /// Spelled as Espressif spells it: the firmware's feature, espflash's `--chip` and
    /// `board-info`'s `Chip type:`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Esp32c5 => "esp32c5",
            Self::Esp32c6 => "esp32c6",
            Self::Esp32p4 => "esp32p4",
        }
    }

    /// IDF's `esp_chip_id_t`, as an image header carries it.
    #[must_use]
    pub const fn image_id(self) -> u16 {
        match self {
            Self::Esp32c5 => 23,
            Self::Esp32c6 => 13,
            Self::Esp32p4 => 18,
        }
    }

    /// The chip an image header's chip ID names, if it is one of [`Self::ALL`].
    #[must_use]
    pub fn from_image_id(id: u16) -> Option<Self> {
        Self::ALL.into_iter().find(|chip| chip.image_id() == id)
    }
}

/// Which firmware an image was checked as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageOf {
    /// A node's.
    Node,
    /// The P4 bridge's.
    Bridge,
}

/// Why an image was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImageError {
    /// Fewer bytes than an image header.
    #[error("too short for an image header")]
    TooShort,
    /// No image header at byte 0.
    #[error("no image header at the start")]
    NoHeader,
    /// No image header past the erased flash ahead of it.
    #[error("no image header where a bootloader starts")]
    NoBootloaderHeader,
    /// The header names chip ID `id`, which is not `chip`.
    #[error("chip ID {id}, not {chip:?}")]
    OtherChip { id: u16, chip: Chip },
    /// The header is at `at`, and an app-only image starts with it.
    #[error("header at {at:#x}, not at the start")]
    NotAtStart { at: usize },
    /// A merged image: bootloader, partition table and app.
    #[error("a merged image")]
    Merged,
    /// The first segment holds no app descriptor.
    #[error("no app descriptor")]
    NoDescriptor,
    /// The descriptor names `project`, which is not the firmware `of`'s.
    #[error("project '{project}', not a {of:?} image")]
    Project { project: String, of: ImageOf },
    /// The descriptor's version is not `major.minor.patch`.
    #[error("version '{version}' is not x.y.z")]
    Version { version: String },
    /// The header names chip ID `id`, and `project` is not built for that chip.
    #[error("chip ID {id} is not {project}'s")]
    ChipOfProject { id: u16, project: String },
    /// Segment `segment` has no header.
    #[error("truncated: segment {segment} has no header")]
    SegmentHeader { segment: u8 },
    /// Segment `segment` runs past the end of the file.
    #[error("truncated: segment {segment} runs past the end")]
    SegmentPastEnd { segment: u8 },
    /// No checksum byte after the segments.
    #[error("truncated: no checksum")]
    NoChecksum,
    /// The checksum byte is `stored`, and the segments give `computed`.
    #[error("checksum {stored:#04x}, segments give {computed:#04x}")]
    Checksum { stored: u8, computed: u8 },
    /// The header says a SHA-256 is appended, and none follows the checksum.
    #[error("truncated: no SHA-256")]
    NoSha256,
    /// The appended SHA-256 is not the contents'.
    #[error("appended SHA-256 does not match")]
    Sha256,
    /// `len` bytes, more than the app slot of the firmware `of` holds.
    #[error("{len} bytes, past the {of:?} app slot")]
    TooLarge { len: usize, of: ImageOf },
    /// No appended digest, which names the image to the P4.
    #[error("no appended digest")]
    NoAppendedDigest,
}

impl ImageOf {
    /// The largest image this firmware's app slot holds.
    #[must_use]
    pub const fn slot_len(self) -> usize {
        match self {
            Self::Node => SLOT_LEN,
            Self::Bridge => BRIDGE_SLOT_LEN,
        }
    }
}

/// An app-only node image, checked, and what its descriptor says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeImage {
    bytes: Arc<[u8]>,
    sha256: [u8; 32],
    version: Version,
    board: Board,
}

impl NodeImage {
    /// Check `bytes` as a node's app-only image and read its descriptor.
    ///
    /// # Errors
    /// When the bytes are not one: no ESP image header, a merged image, no app descriptor, a
    /// project name no node board has, a version that is not `x.y.z`, a chip ID that is not the
    /// board's, a failed checksum or SHA-256, or more than an app slot holds.
    pub fn parse(bytes: Vec<u8>) -> Result<Self, ImageError> {
        let Descriptor { project, version } = descriptor(&bytes)?;
        let board = Board::Esp32c5;
        if project_name(board) != Some(project.as_str()) {
            return Err(ImageError::Project { project, of: ImageOf::Node });
        }
        let Some(version) = parse_version(&version) else {
            return Err(ImageError::Version { version });
        };
        let id = u16::from_le_bytes([bytes[CHIP_ID_AT], bytes[CHIP_ID_AT + 1]]);
        if chip(board).map(Chip::image_id) != Some(id) {
            return Err(ImageError::ChipOfProject { id, project });
        }
        check_integrity(&bytes)?;
        if bytes.len() > SLOT_LEN {
            return Err(ImageError::TooLarge { len: bytes.len(), of: ImageOf::Node });
        }
        let sha256 = Sha256::digest(&bytes).into();
        Ok(Self { bytes: Arc::from(bytes), sha256, version, board })
    }

    /// The image, as an app slot holds it.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Its SHA-256.
    #[must_use]
    pub const fn sha256(&self) -> [u8; 32] {
        self.sha256
    }

    /// The node firmware version it carries.
    #[must_use]
    pub const fn version(&self) -> Version {
        self.version
    }

    /// The board it was built for.
    #[must_use]
    pub const fn board(&self) -> Board {
        self.board
    }

    /// The image as an update session carries it. Its bytes are shared, not copied.
    #[must_use]
    pub fn ota(&self) -> OtaImage {
        OtaImage {
            bytes: Arc::clone(&self.bytes),
            sha256: self.sha256,
            version: self.version,
            board: self.board,
        }
    }
}

/// The P4 bridge's app-only image, checked, with its SHA-256 and the digest that names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeImage {
    bytes: Vec<u8>,
    sha256: [u8; DIGEST_LEN],
    digest: [u8; DIGEST_LEN],
}

impl BridgeImage {
    /// Check `bytes` as the P4 bridge's app-only image.
    ///
    /// # Errors
    /// When the bytes are not one: not built for the P4, not app-only, another project, a failed
    /// checksum or SHA-256, more than the P4's app slot holds, or no appended digest.
    pub fn parse(bytes: Vec<u8>) -> Result<Self, ImageError> {
        let at = find_header(&bytes, Chip::Esp32p4)?;
        if at != 0 {
            return Err(ImageError::NotAtStart { at });
        }
        let project = descriptor(&bytes)?.project;
        if project != BRIDGE_PROJECT {
            return Err(ImageError::Project { project, of: ImageOf::Bridge });
        }
        check_integrity(&bytes)?;
        if bytes.len() > BRIDGE_SLOT_LEN {
            return Err(ImageError::TooLarge { len: bytes.len(), of: ImageOf::Bridge });
        }
        let sha256 = Sha256::digest(&bytes).into();
        let digest = appended_digest_of(&bytes).ok_or(ImageError::NoAppendedDigest)?;
        Ok(Self { bytes, sha256, digest })
    }

    /// The image, as the P4's app slot holds it.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Its SHA-256, which the P4's receiver checks.
    #[must_use]
    pub const fn sha256(&self) -> [u8; DIGEST_LEN] {
        self.sha256
    }

    /// The digest appended to it, which the P4's `Ready` names once it runs the image.
    #[must_use]
    pub const fn digest(&self) -> [u8; DIGEST_LEN] {
        self.digest
    }
}

/// Where the first image header in `image` starts, past the erased flash (`0xFF`) a merged image
/// holds ahead of its bootloader, once the chip ID it names is `chip`'s. A merged image's
/// bootloader sits where its chip boots from, which the caller checks.
///
/// # Errors
/// No header there, too short to hold one, or a header naming another chip.
pub fn find_header(image: &[u8], chip: Chip) -> Result<usize, ImageError> {
    let at = image.iter().take_while(|byte| **byte == 0xFF).count();
    if image.get(at) != Some(&IMAGE_MAGIC) {
        return Err(ImageError::NoBootloaderHeader);
    }
    if image.len() < at + HEADER_LEN {
        return Err(ImageError::TooShort);
    }
    let id = u16::from_le_bytes([image[at + CHIP_ID_AT], image[at + CHIP_ID_AT + 1]]);
    if id != chip.image_id() {
        return Err(ImageError::OtherChip { id, chip });
    }
    Ok(at)
}

/// The descriptor's project name for each board.
pub(super) const fn project_name(board: Board) -> Option<&'static str> {
    match board {
        Board::Esp32c5 => Some("wartui-node-esp32c5"),
        // No node runs on a C6 or a P4.
        Board::Esp32c6 | Board::Esp32p4 | Board::Unknown(_) => None,
    }
}

/// The chip a node on `board` runs on, or `None` for a board no node firmware is built for.
pub(super) const fn chip(board: Board) -> Option<Chip> {
    match board {
        Board::Esp32c5 => Some(Chip::Esp32c5),
        Board::Esp32c6 | Board::Esp32p4 | Board::Unknown(_) => None,
    }
}

/// Whether any node image is built for `board`.
#[must_use]
pub const fn has_node_image(board: Board) -> bool {
    chip(board).is_some()
}

/// What an app image's descriptor says, as text.
struct Descriptor {
    /// The project name, which says which firmware and board the image is for.
    project: String,
    /// The crate version.
    version: String,
}

/// Read the app descriptor at the start of an app-only image's first segment.
fn descriptor(bytes: &[u8]) -> Result<Descriptor, ImageError> {
    if bytes.len() < HEADER_LEN + SEGMENT_HEADER_LEN {
        return Err(ImageError::TooShort);
    }
    if bytes[0] != IMAGE_MAGIC {
        return Err(ImageError::NoHeader);
    }
    let desc_at = HEADER_LEN + SEGMENT_HEADER_LEN;
    let first_segment = le_u32(bytes, HEADER_LEN + 4) as usize;
    let desc = bytes.get(desc_at..desc_at + DESC_PROJECT_AT + DESC_FIELD_LEN);
    let Some(desc) = desc.filter(|desc| {
        le_u32(desc, 0) == APP_DESC_MAGIC && first_segment >= DESC_PROJECT_AT + DESC_FIELD_LEN
    }) else {
        if bytes.get(PARTITION_TABLE_AT..PARTITION_TABLE_AT + 2) == Some(&PARTITION_MAGIC) {
            return Err(ImageError::Merged);
        }
        return Err(ImageError::NoDescriptor);
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
fn check_integrity(bytes: &[u8]) -> Result<(), ImageError> {
    if bytes.len() < HEADER_LEN {
        return Err(ImageError::TooShort);
    }
    let mut at = HEADER_LEN;
    let mut checksum = CHECKSUM_SEED;
    for segment in 0..bytes[SEGMENT_COUNT_AT] {
        let Some(header) = bytes.get(at..at + SEGMENT_HEADER_LEN) else {
            return Err(ImageError::SegmentHeader { segment });
        };
        let len = le_u32(header, 4) as usize;
        let start = at + SEGMENT_HEADER_LEN;
        let Some(data) = bytes.get(start..start + len) else {
            return Err(ImageError::SegmentPastEnd { segment });
        };
        checksum = data.iter().fold(checksum, |sum, byte| sum ^ byte);
        at = start + len;
    }
    let stored_at = at + 15 - at % 16;
    let Some(stored) = bytes.get(stored_at) else {
        return Err(ImageError::NoChecksum);
    };
    if *stored != checksum {
        return Err(ImageError::Checksum { stored: *stored, computed: checksum });
    }
    if bytes[HASH_APPENDED_AT] == 1 {
        let end = stored_at + 1;
        let Some(digest) = bytes.get(end..end + DIGEST_LEN) else {
            return Err(ImageError::NoSha256);
        };
        if Sha256::digest(&bytes[..end])[..] != *digest {
            return Err(ImageError::Sha256);
        }
    }
    Ok(())
}

pub(super) fn le_u32(bytes: &[u8], at: usize) -> u32 {
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::super::testing::{C6_HEADER, P4_APP_HEADER, fixture, fixture_with, seal};
    use super::*;

    /// A P4 bridge app image behind the header espflash wrote for one, `len` bytes long.
    fn p4_app(project: &str, len: usize) -> Vec<u8> {
        fixture_with(P4_APP_HEADER, project, "0.1.0", len)
    }

    /// The `size` of each app slot in the `partitions.csv` of `firmware`.
    fn slot_sizes(firmware: &str) -> Vec<usize> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../firmware").join(firmware);
        let table = std::fs::read_to_string(dir.join("partitions.csv")).unwrap();
        ["ota_0", "ota_1"]
            .into_iter()
            .map(|slot| {
                let row = table.lines().find(|line| line.starts_with(slot)).unwrap();
                let size = row.split(',').map(str::trim).nth(4).unwrap();
                usize::from_str_radix(size.trim_start_matches("0x"), 16).unwrap()
            })
            .collect()
    }

    #[test]
    fn update_image_reads_board_and_version_when_descriptor_is_present() {
        let image = NodeImage::parse(fixture(Board::Esp32c5, "0.1.1", 0x1000)).unwrap();
        assert_eq!((image.board(), image.version()), (Board::Esp32c5, Version::new(0, 1, 1)));
        assert_eq!(image.sha256(), <[u8; 32]>::from(Sha256::digest(image.bytes())));
        let ota = image.ota();
        assert_eq!(
            (ota.sha256, ota.version, ota.board),
            (image.sha256(), image.version(), image.board())
        );
        assert_eq!(&ota.bytes[..], image.bytes());
    }

    #[test]
    fn update_image_reads_espflash_layout_when_given_real_header() {
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
        let image = NodeImage::parse(bytes).unwrap();
        assert_eq!((image.board(), image.version()), (Board::Esp32c5, Version::new(0, 1, 1)));
    }

    #[test]
    fn update_image_refuses_merged_image_when_bootloader_leads() {
        // A bootloader's first segment holds no app descriptor; the partition table at
        // 0x8000 names what it is.
        let mut merged = vec![0xFFu8; 0x9000];
        merged[..24].copy_from_slice(&[0xe9; 24]);
        merged[HEADER_LEN + 4..HEADER_LEN + 8].copy_from_slice(&0x100u32.to_le_bytes());
        merged[PARTITION_TABLE_AT..PARTITION_TABLE_AT + 2].copy_from_slice(&PARTITION_MAGIC);
        assert_eq!(NodeImage::parse(merged), Err(ImageError::Merged));
    }

    #[test]
    fn update_image_refuses_image_when_descriptor_is_missing() {
        let mut bytes = fixture(Board::Esp32c5, "0.1.1", 0x1000);
        bytes[HEADER_LEN + SEGMENT_HEADER_LEN] ^= 0xFF;
        assert_eq!(NodeImage::parse(bytes), Err(ImageError::NoDescriptor));
    }

    #[test]
    fn update_image_refuses_image_when_project_is_no_node_board() {
        let bytes = fixture(Board::Unknown(9), "0.1.1", 0x1000);
        let project = "not-a-node".to_owned();
        assert_eq!(
            NodeImage::parse(bytes),
            Err(ImageError::Project { project, of: ImageOf::Node })
        );
    }

    #[test]
    fn update_image_refuses_image_when_version_is_not_three_numbers() {
        for version in ["0.1", "0.1.1.1", "0.1.x", "0.1.256", ""] {
            let bytes = fixture(Board::Esp32c5, version, 0x1000);
            let refused = ImageError::Version { version: version.to_owned() };
            assert_eq!(NodeImage::parse(bytes), Err(refused), "{version:?} was accepted");
        }
    }

    #[test]
    fn update_image_refuses_image_when_chip_id_is_not_the_boards() {
        let mut bytes = fixture(Board::Esp32c5, "0.1.1", 0x1000);
        bytes[CHIP_ID_AT] = 13;
        let project = "wartui-node-esp32c5".to_owned();
        assert_eq!(NodeImage::parse(bytes), Err(ImageError::ChipOfProject { id: 13, project }));
    }

    #[test]
    fn update_image_refuses_image_when_larger_than_a_slot() {
        assert!(NodeImage::parse(fixture(Board::Esp32c5, "0.1.1", SLOT_LEN)).is_ok());
        let refused = NodeImage::parse(fixture(Board::Esp32c5, "0.1.1", SLOT_LEN + 16));
        assert_eq!(refused, Err(ImageError::TooLarge { len: SLOT_LEN + 16, of: ImageOf::Node }));
    }

    #[test]
    fn update_image_refuses_file_when_too_short_or_not_an_image() {
        assert_eq!(NodeImage::parse(vec![0xe9; 16]), Err(ImageError::TooShort));
        let mut bytes = fixture(Board::Esp32c5, "0.1.1", 0x1000);
        bytes[0] = 0x7F;
        assert_eq!(NodeImage::parse(bytes), Err(ImageError::NoHeader));
    }

    #[test]
    fn update_image_refuses_image_when_segment_byte_is_flipped() {
        let mut bytes = fixture(Board::Esp32c5, "0.1.1", 0x1000);
        bytes[0x800] ^= 0x01;
        assert!(
            matches!(NodeImage::parse(bytes), Err(ImageError::Checksum { .. })),
            "a flipped byte is a checksum failure"
        );
    }

    #[test]
    fn update_image_refuses_image_when_appended_hash_byte_is_flipped() {
        let mut bytes = fixture(Board::Esp32c5, "0.1.1", 0x1000);
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        assert_eq!(NodeImage::parse(bytes), Err(ImageError::Sha256));
    }

    #[test]
    fn update_image_refuses_image_when_truncated() {
        let bytes = fixture(Board::Esp32c5, "0.1.1", 0x1000);
        let refused = NodeImage::parse(bytes[..0x800].to_vec());
        assert_eq!(refused, Err(ImageError::SegmentPastEnd { segment: 0 }));
    }

    #[test]
    fn update_image_refuses_board_when_no_node_firmware_is_built_for_it() {
        assert!(has_node_image(Board::Esp32c5));
        assert!(!has_node_image(Board::Esp32c6));
        assert!(!has_node_image(Board::Unknown(7)));
    }

    #[test]
    fn update_image_matches_node_slot_size_when_reading_partition_table() {
        assert_eq!(slot_sizes("node"), [SLOT_LEN, SLOT_LEN]);
    }

    #[test]
    fn update_image_pins_chip_ids_when_reading_image_headers() {
        // IDF's `esp_chip_id_t`, which espflash's `Chip::id` also carries.
        assert_eq!(Chip::Esp32c6.image_id(), 13);
        assert_eq!(Chip::Esp32c5.image_id(), 23);
        assert_eq!(Chip::Esp32p4.image_id(), 18);
        for chip in Chip::ALL {
            assert_eq!(Chip::from_image_id(chip.image_id()), Some(chip));
        }
        assert_eq!(Chip::from_image_id(99), None);
    }

    #[test]
    fn update_image_accepts_bridge_image_when_it_is_the_p4_bridge_app() {
        let image = BridgeImage::parse(p4_app(BRIDGE_PROJECT, 0x1000)).unwrap();
        assert_eq!(image.sha256(), <[u8; 32]>::from(Sha256::digest(image.bytes())));
        assert_eq!(Some(image.digest()), appended_digest_of(image.bytes()));
        BridgeImage::parse(p4_app(BRIDGE_PROJECT, BRIDGE_SLOT_LEN)).unwrap();
    }

    #[test]
    fn update_image_refuses_bridge_image_when_project_is_not_the_p4_bridge() {
        let refused = BridgeImage::parse(p4_app("wartui-node-esp32c6", 0x1000));
        let project = "wartui-node-esp32c6".to_owned();
        assert_eq!(refused, Err(ImageError::Project { project, of: ImageOf::Bridge }));
    }

    #[test]
    fn update_image_refuses_bridge_image_when_truncated() {
        let image = p4_app(BRIDGE_PROJECT, 0x1000);
        let refused = BridgeImage::parse(image[..0x800].to_vec());
        assert_eq!(refused, Err(ImageError::SegmentPastEnd { segment: 0 }));
    }

    #[test]
    fn update_image_refuses_bridge_image_when_segment_byte_is_flipped() {
        let mut image = p4_app(BRIDGE_PROJECT, 0x1000);
        image[0x800] ^= 0x01;
        assert!(matches!(BridgeImage::parse(image), Err(ImageError::Checksum { .. })));
    }

    #[test]
    fn update_image_refuses_bridge_image_when_larger_than_its_slot() {
        let refused = BridgeImage::parse(p4_app(BRIDGE_PROJECT, BRIDGE_SLOT_LEN + 16));
        let len = BRIDGE_SLOT_LEN + 16;
        assert_eq!(refused, Err(ImageError::TooLarge { len, of: ImageOf::Bridge }));
    }

    #[test]
    fn update_image_refuses_bridge_image_when_built_for_another_chip() {
        let image = fixture_with(C6_HEADER, "wartui-p4-radio", "0.1.0", 0x1000);
        let refused = BridgeImage::parse(image);
        assert_eq!(refused, Err(ImageError::OtherChip { id: 13, chip: Chip::Esp32p4 }));
    }

    #[test]
    fn update_image_refuses_bridge_image_when_given_a_merged_one() {
        // A merged P4 image, its bootloader behind 0x2000 of erased flash.
        let mut merged = vec![0xFF; 0x2000];
        merged.extend_from_slice(&p4_app(BRIDGE_PROJECT, 0x1000));
        assert_eq!(BridgeImage::parse(merged), Err(ImageError::NotAtStart { at: 0x2000 }));
    }

    #[test]
    fn update_image_refuses_bridge_image_when_it_carries_no_appended_digest() {
        // An image with no SHA-256 appended carries no digest either.
        let mut image = p4_app(BRIDGE_PROJECT, 0x1000);
        image[HASH_APPENDED_AT] = 0;
        image.truncate(image.len() - DIGEST_LEN);
        assert_eq!(BridgeImage::parse(image), Err(ImageError::NoAppendedDigest));
    }

    #[test]
    fn update_image_matches_bridge_slot_size_when_reading_partition_table() {
        assert_eq!(slot_sizes("p4-bridge"), [BRIDGE_SLOT_LEN, BRIDGE_SLOT_LEN]);
    }

    #[test]
    fn update_image_finds_header_when_erased_flash_leads() {
        let mut merged = vec![0xFF; 0x2000];
        merged.extend_from_slice(&P4_APP_HEADER);
        assert_eq!(find_header(&merged, Chip::Esp32p4), Ok(0x2000));
        assert_eq!(find_header(&merged[..0x2000 + 8], Chip::Esp32p4), Err(ImageError::TooShort));
        assert_eq!(find_header(&[], Chip::Esp32p4), Err(ImageError::NoBootloaderHeader));
    }
}
