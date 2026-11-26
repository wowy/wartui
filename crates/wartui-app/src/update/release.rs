//! A release's app-only images: fetched, checked against the release's `SHA256SUMS`, and cached.
//!
//! **A release pins its images by tag, and the tag is the caller's.** A front end built for a
//! release fetches that release's image, checks it against the same release's `SHA256SUMS`, and
//! keeps both under its cache directory, so a laptop with no signal can still update. One
//! `SHA256SUMS` covers every image of a release, so every firmware shares one cache directory per
//! tag. A cached copy is used while it still matches the cached `SHA256SUMS`, and an image that
//! does not match is never cached.
//!
//! **A release download is redirected**, from the repository to where its assets are stored, so
//! the request follows redirects, as `upload`'s agent deliberately does not.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use wartui_proto::bus::Board;

/// The release asset naming every other asset's digest.
pub const SUMS: &str = "SHA256SUMS";

/// The node's app-only image, which an over-the-air update carries. Must match `release.yml`.
pub const NODE_APP_ASSET: &str = "wartui-node-app-esp32c5.bin";

/// The P4 bridge's app-only image, which its update carries. Must match `release.yml`.
pub const BRIDGE_APP_ASSET: &str = "wartui-p4-bridge-app-esp32p4.bin";

/// The largest asset a download reads.
const BODY_LIMIT: u64 = 16 * 1024 * 1024;

/// The release asset of `board`'s app-only node image, or `None` for a board no node firmware is
/// built for.
#[must_use]
pub const fn node_asset(board: Board) -> Option<&'static str> {
    match board {
        Board::Esp32c5 => Some(NODE_APP_ASSET),
        Board::Esp32c6 | Board::Esp32p4 | Board::Unknown(_) => None,
    }
}

/// Where releases are published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Releases {
    /// The URL each release's tag and asset follow.
    pub base: String,
}

impl Releases {
    /// This repository's releases.
    #[must_use]
    pub fn live() -> Self {
        Self { base: concat!(env!("CARGO_PKG_REPOSITORY"), "/releases/download").to_owned() }
    }

    /// Where release `tag` keeps `asset`.
    #[must_use]
    pub fn url(&self, tag: &str, asset: &str) -> String {
        format!("{}/{tag}/{asset}", self.base)
    }
}

/// A release asset on disk, checked against its release's `SHA256SUMS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cached {
    path: PathBuf,
    bytes: Vec<u8>,
}

impl Cached {
    /// Where the cache keeps it.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Its contents.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Its contents, taken.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// Why a release asset could not be had.
#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    /// The request for `url` failed, in the HTTP client's words.
    #[error("fetching {url}: {error}")]
    Fetch { url: String, error: String },
    /// The answer from `url` could not be read, in the HTTP client's words.
    #[error("reading {url}: {error}")]
    Read { url: String, error: String },
    /// `SHA256SUMS` is not UTF-8.
    #[error("{SUMS} is not text")]
    SumsNotText(#[source] std::string::FromUtf8Error),
    /// `SHA256SUMS` lists no digest for `asset`.
    #[error("{SUMS} does not list {asset}")]
    Unlisted { asset: String },
    /// `asset`'s digest is `actual`, and `SHA256SUMS` lists `listed`.
    #[error("{asset} is {actual}, {SUMS} says {listed}")]
    Mismatch { asset: String, actual: String, listed: String },
    /// The cache directory `dir` could not be created.
    #[error("creating {}", dir.display())]
    CreateDir {
        dir: PathBuf,
        #[source]
        error: std::io::Error,
    },
    /// `path` could not be written into the cache.
    #[error("writing {}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        error: std::io::Error,
    },
}

/// Release `tag`'s `asset`, from `cache/tag/` when the copy there still matches the cached
/// `SHA256SUMS`, else fetched from `releases` with its `SHA256SUMS`, checked, and cached. Blocks
/// on the network.
///
/// # Errors
/// A fetch failed, the sums are not text or do not list the asset, the asset is not the digest
/// they list, or the cache could not be written.
pub fn fetch(
    releases: &Releases,
    tag: &str,
    asset: &str,
    cache: &Path,
) -> Result<Cached, ReleaseError> {
    fetch_with(releases, tag, asset, cache, &http_get)
}

/// [`fetch`], with `get` as the network, so a test can be without one.
fn fetch_with(
    releases: &Releases,
    tag: &str,
    asset: &str,
    cache: &Path,
    get: &dyn Fn(&str) -> Result<Vec<u8>, ReleaseError>,
) -> Result<Cached, ReleaseError> {
    let dir = cache.join(tag);
    let (sums_path, path) = (dir.join(SUMS), dir.join(asset));
    if let (Ok(sums), Ok(bytes)) = (std::fs::read_to_string(&sums_path), std::fs::read(&path))
        && verify(&bytes, &sums, asset).is_ok()
    {
        return Ok(Cached { path, bytes });
    }
    let sums = get(&releases.url(tag, SUMS))?;
    let sums = String::from_utf8(sums).map_err(ReleaseError::SumsNotText)?;
    let bytes = get(&releases.url(tag, asset))?;
    verify(&bytes, &sums, asset)?;
    std::fs::create_dir_all(&dir).map_err(|error| ReleaseError::CreateDir { dir, error })?;
    std::fs::write(&sums_path, &sums)
        .map_err(|error| ReleaseError::Write { path: sums_path, error })?;
    std::fs::write(&path, &bytes)
        .map_err(|error| ReleaseError::Write { path: path.clone(), error })?;
    Ok(Cached { path, bytes })
}

/// The digest `SHA256SUMS` gives for `asset`, in the `sha256sum` format: a hex digest, a space,
/// then a space or a `*`, then the name.
fn listed_digest<'a>(sums: &'a str, asset: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let (digest, rest) = line.trim().split_once(' ')?;
        let name = rest.trim_start_matches([' ', '*']);
        (name == asset).then_some(digest)
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Refuse `bytes` unless `SHA256SUMS` lists exactly their digest for `asset`.
fn verify(bytes: &[u8], sums: &str, asset: &str) -> Result<(), ReleaseError> {
    let Some(listed) = listed_digest(sums, asset) else {
        return Err(ReleaseError::Unlisted { asset: asset.to_owned() });
    };
    let actual = sha256_hex(bytes);
    if !listed.eq_ignore_ascii_case(&actual) {
        let (asset, listed) = (asset.to_owned(), listed.to_owned());
        return Err(ReleaseError::Mismatch { asset, actual, listed });
    }
    Ok(())
}

/// Get `url`, following redirects, reading at most [`BODY_LIMIT`].
fn http_get(url: &str) -> Result<Vec<u8>, ReleaseError> {
    let failed = |error: ureq::Error| error.to_string();
    let mut response = ureq::get(url)
        .call()
        .map_err(|error| ReleaseError::Fetch { url: url.to_owned(), error: failed(error) })?;
    response
        .body_mut()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_vec()
        .map_err(|error| ReleaseError::Read { url: url.to_owned(), error: failed(error) })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::super::testing::fixture;
    use super::*;

    fn test_releases() -> Releases {
        Releases { base: "http://test".to_owned() }
    }

    fn image() -> Vec<u8> {
        fixture(Board::Esp32c5, "0.1.1", 0x1000)
    }

    #[test]
    fn update_release_builds_download_url_when_given_tag_and_asset() {
        let url = Releases::live().url("v0.2.0", "wartui-node-fw-esp32c5.bin");
        assert_eq!(
            url,
            "https://github.com/wowy/wartui/releases/download/v0.2.0/wartui-node-fw-esp32c5.bin"
        );
    }

    #[test]
    fn update_release_names_node_asset_when_board_has_node_firmware() {
        assert_eq!(node_asset(Board::Esp32c5), Some("wartui-node-app-esp32c5.bin"));
        assert_eq!(node_asset(Board::Esp32c6), None);
        assert_eq!(node_asset(Board::Unknown(7)), None);
    }

    #[test]
    fn update_release_verifies_digest_when_sums_list_the_asset() {
        let sums = format!("{}  a.bin\n{} *b.bin\n", sha256_hex(b"a"), sha256_hex(b"b"));
        verify(b"a", &sums, "a.bin").unwrap();
        verify(b"b", &sums, "b.bin").unwrap();
    }

    #[test]
    fn update_release_rejects_download_when_digest_mismatches() {
        let sums = format!("{}  a.bin\n", sha256_hex(b"a"));
        let Err(ReleaseError::Mismatch { asset, actual, listed }) =
            verify(b"tampered", &sums, "a.bin")
        else {
            panic!("a tampered download was accepted")
        };
        assert_eq!(
            (asset.as_str(), actual, listed),
            ("a.bin", sha256_hex(b"tampered"), sha256_hex(b"a"))
        );
        let unlisted = verify(b"a", &sums, "missing.bin");
        assert!(
            matches!(unlisted, Err(ReleaseError::Unlisted { asset }) if asset == "missing.bin")
        );
    }

    #[test]
    fn update_release_fetches_and_caches_image_when_cache_is_empty() {
        let cache = tempfile::tempdir().unwrap();
        let image = image();
        let sums = format!("{}  x.bin\n", sha256_hex(&image));
        let asked = RefCell::new(Vec::new());
        let get = |url: &str| -> Result<Vec<u8>, ReleaseError> {
            asked.borrow_mut().push(url.to_owned());
            Ok(if url.ends_with(SUMS) { sums.clone().into_bytes() } else { image.clone() })
        };
        let cached = fetch_with(&test_releases(), "v1", "x.bin", cache.path(), &get).unwrap();
        assert_eq!(std::fs::read(cached.path()).unwrap(), image);
        assert_eq!(cached.bytes(), image);
        assert_eq!(*asked.borrow(), ["http://test/v1/SHA256SUMS", "http://test/v1/x.bin"]);
    }

    #[test]
    fn update_release_uses_cached_image_when_offline() {
        let cache = tempfile::tempdir().unwrap();
        let dir = cache.path().join("v1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.bin"), image()).unwrap();
        std::fs::write(dir.join(SUMS), format!("{}  x.bin\n", sha256_hex(&image()))).unwrap();
        let offline = |url: &str| -> Result<Vec<u8>, ReleaseError> {
            Err(ReleaseError::Fetch { url: url.to_owned(), error: "offline".to_owned() })
        };
        let cached = fetch_with(&test_releases(), "v1", "x.bin", cache.path(), &offline).unwrap();
        assert_eq!(cached.path(), dir.join("x.bin"));
        assert_eq!(cached.into_bytes(), image());
    }

    #[test]
    fn update_release_rejects_download_when_fetched_image_mismatches_sums() {
        let cache = tempfile::tempdir().unwrap();
        let sums = format!("{}  x.bin\n", sha256_hex(b"the real one"));
        let get = |url: &str| -> Result<Vec<u8>, ReleaseError> {
            Ok(if url.ends_with(SUMS) { sums.clone().into_bytes() } else { b"another".to_vec() })
        };
        let fetched = fetch_with(&test_releases(), "v1", "x.bin", cache.path(), &get);
        assert!(matches!(fetched, Err(ReleaseError::Mismatch { .. })), "{fetched:?}");
        assert!(!cache.path().join("v1/x.bin").exists(), "a rejected image is not cached");
    }
}
