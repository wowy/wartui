//! The node image cache's rules, which the P4 and the simulator share.

use sha2::{Digest, Sha256};
use wartui_proto::bus::{UPDATE_CHUNK, UpdateState};
use wartui_proto::link::{NodeImageCache, image_tag};
use wartui_proto::ota::{Receiver, SECTOR};
use wartui_proto::radio_link::update::Op;

type Cache = NodeImageCache<Box<Receiver>, Vec<u8>>;

/// A cache over a region of `len` bytes.
fn cache(len: usize) -> Cache {
    NodeImageCache::new(Box::new(Receiver::new(wartui_proto::bus::Board::Esp32c5)), vec![0; len])
}

/// Two sectors and a bit, so the upload ends on a partial sector.
fn image(seed: u8) -> (Vec<u8>, [u8; 32]) {
    let bytes: Vec<u8> = (0..2 * SECTOR + 100).map(|i| (i % 251) as u8 ^ seed).collect();
    let sha256 = Sha256::digest(&bytes).into();
    (bytes, sha256)
}

/// Open an upload of `sha256`'s image, `len` bytes long, returning the state it answers.
fn begin(cache: &mut Cache, session: u8, len: usize, sha256: [u8; 32]) -> UpdateState {
    let len = u32::try_from(len).expect("a small image");
    cache.take(Op::Begin { session, len, sha256 }).expect("a status").state
}

/// Upload `bytes` whole, returning the state the commit answers.
fn upload(cache: &mut Cache, session: u8, bytes: &[u8], sha256: [u8; 32]) -> UpdateState {
    assert_eq!(begin(cache, session, bytes.len(), sha256), UpdateState::Receiving);
    for (i, data) in bytes.chunks(UPDATE_CHUNK).enumerate() {
        let offset = u32::try_from(i * UPDATE_CHUNK).expect("a small image");
        let _ = cache.take(Op::Chunk { session, offset, data });
    }
    cache.take(Op::Commit { session }).expect("a status").state
}

#[test]
fn node_image_cache_serves_nothing_when_nothing_is_uploaded() {
    let (_, sha256) = image(0);
    assert_eq!(cache(2 * 1024 * 1024).image(image_tag(&sha256)), None);
}

#[test]
fn node_image_cache_serves_image_when_upload_verifies() {
    let mut cache = cache(2 * 1024 * 1024);
    let (bytes, sha256) = image(0);
    assert_eq!(upload(&mut cache, 1, &bytes, sha256), UpdateState::Verified);
    assert_eq!(cache.image(image_tag(&sha256)), Some(&bytes[..]));
}

#[test]
fn node_image_cache_serves_nothing_when_tag_is_another_image() {
    let mut cache = cache(2 * 1024 * 1024);
    let (bytes, sha256) = image(0);
    upload(&mut cache, 1, &bytes, sha256);
    let (_, other) = image(1);
    assert_eq!(cache.image(image_tag(&other)), None);
}

#[test]
fn node_image_cache_serves_nothing_when_a_new_upload_begins() {
    let mut cache = cache(2 * 1024 * 1024);
    let (first, first_sha) = image(0);
    upload(&mut cache, 1, &first, first_sha);
    let (second, second_sha) = image(1);
    assert_eq!(begin(&mut cache, 2, second.len(), second_sha), UpdateState::Receiving);
    // Neither the image the upload replaces nor the half-written one is served.
    assert_eq!(cache.image(image_tag(&first_sha)), None);
    assert_eq!(cache.image(image_tag(&second_sha)), None);
}

#[test]
fn node_image_cache_keeps_its_image_when_a_begin_is_refused() {
    let mut cache = cache(2 * 1024 * 1024);
    let (bytes, sha256) = image(0);
    upload(&mut cache, 1, &bytes, sha256);
    let (_, other) = image(1);
    assert_eq!(begin(&mut cache, 2, 4 * 1024 * 1024, other), UpdateState::TooLarge);
    assert_eq!(cache.image(image_tag(&sha256)), Some(&bytes[..]));
    assert_eq!(cache.image(image_tag(&other)), None);
}

#[test]
fn node_image_cache_answers_verified_when_begin_names_the_image_it_holds() {
    let mut cache = cache(2 * 1024 * 1024);
    let (bytes, sha256) = image(0);
    upload(&mut cache, 1, &bytes, sha256);
    assert_eq!(begin(&mut cache, 1, bytes.len(), sha256), UpdateState::Verified);
    assert_eq!(cache.image(image_tag(&sha256)), Some(&bytes[..]));
}

#[test]
fn node_image_cache_refuses_install_when_image_is_verified() {
    let mut cache = cache(2 * 1024 * 1024);
    let (bytes, sha256) = image(0);
    upload(&mut cache, 1, &bytes, sha256);
    let status = cache.take(Op::Install { session: 1 }).expect("a status");
    assert_eq!(status.state, UpdateState::FlashError);
    // A cached image only feeds chunks, and still does.
    assert_eq!(cache.image(image_tag(&sha256)), Some(&bytes[..]));
}

#[test]
fn node_image_cache_refuses_begin_when_region_is_empty() {
    let mut cache = cache(0);
    let (bytes, sha256) = image(0);
    assert_eq!(cache.capacity(), 0);
    assert_eq!(begin(&mut cache, 1, bytes.len(), sha256), UpdateState::FlashError);
    assert_eq!(cache.image(image_tag(&sha256)), None);
}

#[test]
fn node_image_cache_serves_nothing_when_reset() {
    let mut cache = cache(2 * 1024 * 1024);
    let (bytes, sha256) = image(0);
    upload(&mut cache, 1, &bytes, sha256);
    cache.reset();
    assert_eq!(cache.image(image_tag(&sha256)), None);
}
