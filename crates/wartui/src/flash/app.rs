//! The app-only images an update carries, as the command line finds them for each board, and
//! the words for why one was refused.
//!
//! **The image comes from the sources `flash-fleet` uses**: `--image`, a build of the checkout
//! (saved without `--merge`), or this binary's release, checked against its `SHA256SUMS`. What
//! an image must be before it is sent, and why, is `wartui_app::update::image`'s.

use std::path::Path;

use anyhow::{Result, anyhow, ensure};
use wartui_app::update::image::{
    BRIDGE_PROJECT, BRIDGE_SLOT_LEN, Chip, ImageError, ImageOf, NodeImage, SLOT_LEN, has_node_image,
};
use wartui_proto::bus::Board;

use super::{NODE_APP, RELEASE_TAG, Source, fetch, read, source};

/// A board as an operator reads it.
#[must_use]
pub fn board_name(board: Board) -> String {
    match board {
        Board::Esp32c5 => "esp32c5".to_owned(),
        Board::Esp32c6 => "esp32c6".to_owned(),
        Board::Esp32p4 => "esp32p4".to_owned(),
        Board::Unknown(byte) => format!("unknown board {byte}"),
    }
}

/// Why an image was refused, as an operator reads it.
pub fn image_error_text(error: &ImageError) -> String {
    match error {
        ImageError::TooShort => "too short to hold an image header".to_owned(),
        ImageError::NoHeader => "not an ESP image: no header at the start".to_owned(),
        ImageError::NoBootloaderHeader => {
            "not an ESP image: no header where a bootloader should start".to_owned()
        }
        ImageError::OtherChip { id, chip } => {
            let built_for = Chip::from_image_id(*id)
                .map_or_else(|| format!("chip ID {id}"), |other| other.name().to_owned());
            format!("the image is built for {built_for}, not {}", chip.name())
        }
        ImageError::NotAtStart { at } => {
            format!("the image's header is at {at:#x}, and an app-only image starts with it")
        }
        ImageError::Merged => {
            "a merged image (bootloader, partition table and app); use the app-only image"
                .to_owned()
        }
        ImageError::NoDescriptor => {
            "no app descriptor at the start of the first segment".to_owned()
        }
        ImageError::Project { project, of: ImageOf::Node } => {
            format!("built for project '{project}', which is no node board")
        }
        ImageError::Project { project, of: ImageOf::Bridge } => {
            format!("built for project '{project}', and a P4 bridge runs {BRIDGE_PROJECT}")
        }
        ImageError::Version { version } => {
            format!("its version '{version}' is not major.minor.patch")
        }
        ImageError::ChipOfProject { id, project } => {
            format!("its header names chip ID {id}, and {project} is not that chip")
        }
        ImageError::SegmentHeader { segment } => {
            format!("the image is truncated: segment {segment} has no header")
        }
        ImageError::SegmentPastEnd { segment } => {
            format!("the image is truncated: segment {segment} runs past its end")
        }
        ImageError::NoChecksum => {
            "the image is truncated: no checksum after its segments".to_owned()
        }
        ImageError::Checksum { stored, computed } => format!(
            "the image is corrupt: its checksum byte is {stored:#04x}, and its segments give \
             {computed:#04x}"
        ),
        ImageError::NoSha256 => "the image is truncated: no SHA-256 after its checksum".to_owned(),
        ImageError::Sha256 => {
            "the image is corrupt: its appended SHA-256 does not match its contents".to_owned()
        }
        ImageError::TooLarge { len, of: ImageOf::Node } => {
            format!("{len} bytes, and an app slot holds {SLOT_LEN}")
        }
        ImageError::TooLarge { len, of: ImageOf::Bridge } => {
            format!("{len} bytes, and the P4's app slot holds {BRIDGE_SLOT_LEN}")
        }
        ImageError::NoAppendedDigest => {
            "it carries no appended digest, which names the image to the P4".to_owned()
        }
    }
}

/// `parsed`, the image at `path` checked, with a refusal worded for the operator: in the context
/// of checking `path`, or naming `path` when the image carries no digest.
///
/// # Errors
/// The refusal.
pub fn checked<T>(path: &Path, parsed: Result<T, ImageError>) -> Result<T> {
    parsed.map_err(|error| match error {
        ImageError::NoAppendedDigest => anyhow!(
            "{} carries no appended digest, which names the image to the P4",
            path.display()
        ),
        error => anyhow!(image_error_text(&error)).context(format!("checking {}", path.display())),
    })
}

/// Read and check the node image at `path`.
///
/// # Errors
/// When it cannot be read, or is not a node's app-only image.
pub fn load(path: &Path) -> Result<NodeImage> {
    checked(path, NodeImage::parse(read(path)?))
}

/// Where `board`'s image comes from: a build of `firmware_dir` or the checkout, else this
/// binary's release.
///
/// # Errors
/// For a board no node firmware is built for.
pub fn source_for(board: Board, firmware_dir: Option<&Path>) -> Result<Source> {
    ensure!(has_node_image(board), "no node firmware is built for {}", board_name(board));
    Ok(source(&NODE_APP, None, firmware_dir, RELEASE_TAG))
}

/// Build or fetch `board`'s image from `source`, and check it is that board's.
///
/// # Errors
/// When the build or download fails, or the image is not `board`'s.
pub fn obtain(board: Board, source: &Source) -> Result<NodeImage> {
    ensure!(has_node_image(board), "no node firmware is built for {}", board_name(board));
    let image = load(&fetch(&NODE_APP, source)?)?;
    ensure!(
        image.board() == board,
        "the image built for {} says it is for {}",
        board_name(board),
        board_name(image.board())
    );
    Ok(image)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use wartui_app::update::testing::fixture;

    use super::*;

    #[test]
    fn app_image_fetches_release_asset_when_built_for_a_release() {
        // The name `release.yml` publishes the node's app-only image under.
        let asset = "wartui-node-app-esp32c5.bin";
        assert_eq!(
            source(&NODE_APP, None, None, Some("v1")),
            Source::Release { tag: "v1".to_owned(), asset }
        );
    }

    #[test]
    fn app_image_chooses_source_when_given_firmware_dir_or_none() {
        let dir = Path::new("fw");
        assert_eq!(source(&NODE_APP, None, Some(dir), Some("v1")), Source::Build(dir.to_owned()));
        let Source::Build(checkout) = source(&NODE_APP, None, None, None) else {
            panic!("not a build")
        };
        assert!(checkout.ends_with("firmware/node"), "{}", checkout.display());
    }

    #[test]
    fn app_image_refuses_source_when_no_node_firmware_is_built_for_board() {
        assert!(source_for(Board::Esp32c5, None).is_ok());
        assert!(source_for(Board::Unknown(7), None).is_err());
        assert!(source_for(Board::Esp32c6, None).is_err());
    }

    /// Why `bytes` are no node image, as the operator reads it.
    fn node_refusal(bytes: Vec<u8>) -> String {
        image_error_text(&NodeImage::parse(bytes).unwrap_err())
    }

    #[test]
    fn app_image_words_refusal_when_image_is_merged() {
        let error = image_error_text(&ImageError::Merged);
        assert!(error.contains("merged image"), "{error}");
    }

    #[test]
    fn app_image_words_refusal_when_descriptor_is_missing() {
        assert!(image_error_text(&ImageError::NoDescriptor).contains("no app descriptor"));
    }

    #[test]
    fn app_image_words_refusal_when_project_is_no_node_board() {
        let error = node_refusal(fixture(Board::Unknown(9), "0.1.1", 0x1000));
        assert_eq!(error, "built for project 'not-a-node', which is no node board");
    }

    #[test]
    fn app_image_words_refusal_when_version_is_not_three_numbers() {
        let error = node_refusal(fixture(Board::Esp32c5, "0.1", 0x1000));
        assert_eq!(error, "its version '0.1' is not major.minor.patch");
    }

    #[test]
    fn app_image_words_refusal_when_chip_id_is_not_the_boards() {
        let mut bytes = fixture(Board::Esp32c5, "0.1.1", 0x1000);
        bytes[12] = 13;
        let error = node_refusal(bytes);
        assert_eq!(error, "its header names chip ID 13, and wartui-node-esp32c5 is not that chip");
    }

    #[test]
    fn app_image_words_refusal_when_larger_than_a_slot() {
        let error = node_refusal(fixture(Board::Esp32c5, "0.1.1", SLOT_LEN + 16));
        assert_eq!(error, format!("{} bytes, and an app slot holds {SLOT_LEN}", SLOT_LEN + 16));
        let error = image_error_text(&ImageError::TooLarge { len: 7, of: ImageOf::Bridge });
        assert_eq!(error, format!("7 bytes, and the P4's app slot holds {BRIDGE_SLOT_LEN}"));
    }

    #[test]
    fn app_image_words_refusal_when_too_short_or_not_an_image() {
        assert_eq!(node_refusal(vec![0xe9; 16]), "too short to hold an image header");
        let mut bytes = fixture(Board::Esp32c5, "0.1.1", 0x1000);
        bytes[0] = 0x7F;
        assert_eq!(node_refusal(bytes), "not an ESP image: no header at the start");
    }

    #[test]
    fn app_image_words_refusal_when_segment_byte_is_flipped() {
        let mut bytes = fixture(Board::Esp32c5, "0.1.1", 0x1000);
        bytes[0x800] ^= 0x01;
        let error = node_refusal(bytes);
        assert!(error.contains("corrupt") && error.contains("checksum"), "{error}");
        let error = image_error_text(&ImageError::Checksum { stored: 0x0a, computed: 0xef });
        assert_eq!(
            error,
            "the image is corrupt: its checksum byte is 0x0a, and its segments give 0xef"
        );
    }

    #[test]
    fn app_image_words_refusal_when_appended_hash_byte_is_flipped() {
        let mut bytes = fixture(Board::Esp32c5, "0.1.1", 0x1000);
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        assert!(node_refusal(bytes).contains("SHA-256"));
    }

    #[test]
    fn app_image_words_refusal_when_truncated() {
        let bytes = fixture(Board::Esp32c5, "0.1.1", 0x1000);
        let error = node_refusal(bytes[..0x800].to_vec());
        assert_eq!(error, "the image is truncated: segment 0 runs past its end");
        for (refused, text) in [
            (ImageError::SegmentHeader { segment: 1 }, "segment 1 has no header"),
            (ImageError::NoChecksum, "no checksum after its segments"),
            (ImageError::NoSha256, "no SHA-256 after its checksum"),
        ] {
            assert_eq!(image_error_text(&refused), format!("the image is truncated: {text}"));
        }
    }

    #[test]
    fn app_image_words_refusal_when_built_for_another_chip_or_merged() {
        let other = ImageError::OtherChip { id: 13, chip: Chip::Esp32p4 };
        assert_eq!(image_error_text(&other), "the image is built for esp32c6, not esp32p4");
        let unknown = ImageError::OtherChip { id: 99, chip: Chip::Esp32p4 };
        assert_eq!(image_error_text(&unknown), "the image is built for chip ID 99, not esp32p4");
        let text = image_error_text(&ImageError::NotAtStart { at: 0x2000 });
        assert_eq!(text, "the image's header is at 0x2000, and an app-only image starts with it");
        let text = image_error_text(&ImageError::NoBootloaderHeader);
        assert_eq!(text, "not an ESP image: no header where a bootloader should start");
    }

    #[test]
    fn app_image_words_refusal_when_project_is_not_the_p4_bridge() {
        let project = "wartui-node-esp32c6".to_owned();
        let error = image_error_text(&ImageError::Project { project, of: ImageOf::Bridge });
        assert_eq!(
            error,
            "built for project 'wartui-node-esp32c6', and a P4 bridge runs wartui-p4-bridge-fw"
        );
    }

    #[test]
    fn app_image_names_the_file_when_refusing_it() {
        let path = PathBuf::from("fw.bin");
        let error = checked::<()>(&path, Err(ImageError::Sha256)).unwrap_err();
        assert_eq!(error.to_string(), "checking fw.bin");
        let cause = error.chain().nth(1).map(ToString::to_string);
        let expected = "the image is corrupt: its appended SHA-256 does not match its contents";
        assert_eq!(cause.as_deref(), Some(expected));
        let error = checked::<()>(&path, Err(ImageError::NoAppendedDigest)).unwrap_err();
        assert_eq!(
            format!("{error:?}"),
            "fw.bin carries no appended digest, which names the image to the P4"
        );
    }
}
