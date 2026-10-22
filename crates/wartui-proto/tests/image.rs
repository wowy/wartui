//! The digest espflash appends to an app image, read from a synthetic one.

use sha2::{Digest, Sha256};
use wartui_proto::ota::image::appended_digest;

/// An app image of `segments` (each of that many data bytes), with `hash_appended` set as
/// given and the digest appended when it is.
fn app_image(segments: &[usize], hash_appended: bool) -> Vec<u8> {
    let mut image = vec![0u8; 24];
    image[0] = 0xE9;
    image[1] = segments.len() as u8;
    image[23] = u8::from(hash_appended);
    for (i, &len) in segments.iter().enumerate() {
        image.extend_from_slice(&(0x4080_0000u32 + i as u32).to_le_bytes());
        image.extend_from_slice(&(len as u32).to_le_bytes());
        image.extend((0..len).map(|b| (b % 253) as u8 + 1));
    }
    // Pad so the checksum byte ends a 16-byte block.
    while image.len() % 16 != 15 {
        image.push(0);
    }
    image.push(0xEF);
    if hash_appended {
        let digest: [u8; 32] = Sha256::digest(&image).into();
        image.extend_from_slice(&digest);
    }
    image
}

fn reader(image: &[u8]) -> impl FnMut(u32, &mut [u8]) -> bool + '_ {
    move |at, buf| match image.get(at as usize..at as usize + buf.len()) {
        Some(bytes) => {
            buf.copy_from_slice(bytes);
            true
        }
        None => false,
    }
}

#[test]
fn image_digest_finds_appended_digest_when_image_has_several_segments() {
    for segments in [&[5usize, 100, 17][..], &[16], &[0, 3]] {
        let image = app_image(segments, true);
        let (head, tail) = image.split_at(image.len() - 32);
        let digest: [u8; 32] = Sha256::digest(head).into();
        assert_eq!(tail, digest);
        assert_eq!(appended_digest(reader(&image)), Some(digest), "{segments:?}");
    }
}

#[test]
fn image_digest_finds_digest_when_slot_continues_past_image() {
    // A slot holds the image and whatever follows it on flash.
    let mut image = app_image(&[40, 2], true);
    let digest = image[image.len() - 32..].to_vec();
    image.extend_from_slice(&[0xFF; 4096]);
    assert_eq!(appended_digest(reader(&image)).map(|d| d.to_vec()), Some(digest));
}

#[test]
fn image_digest_returns_none_when_hash_appended_is_zero() {
    let image = app_image(&[5, 100], false);
    assert_eq!(appended_digest(reader(&image)), None);
}

#[test]
fn image_digest_returns_none_when_magic_is_wrong() {
    let mut image = app_image(&[5, 100], true);
    image[0] = 0xE8;
    assert_eq!(appended_digest(reader(&image)), None);
}

#[test]
fn image_digest_returns_none_when_header_is_truncated() {
    let image = app_image(&[5, 100], true);
    assert_eq!(appended_digest(reader(&image[..20])), None);
}

#[test]
fn image_digest_returns_none_when_image_ends_before_digest() {
    let image = app_image(&[5, 100], true);
    assert_eq!(appended_digest(reader(&image[..image.len() - 1])), None);
    assert_eq!(appended_digest(reader(&image[..60])), None);
}

#[test]
fn image_digest_returns_none_when_segment_count_is_zero_or_past_sixteen() {
    for count in [0u8, 17] {
        let mut image = app_image(&[5], true);
        image[1] = count;
        assert_eq!(appended_digest(reader(&image)), None, "{count}");
    }
}
