//! The node image cache: one node image in PSRAM, which the P4 builds every node's
//! `UpdateChunk` from.
//!
//! Every node on a board takes the same image, so `wartui ota` sends it over the host link once,
//! as [`HostToBridge::CacheImage`] ops in the `wartui_proto::radio_link::update` layout. The P4
//! drives a [`Receiver`] over the PSRAM with them, as it does over flash for its own image
//! (`src/update.rs`), and answers each status as a priority [`BridgeToHost::CacheImage`]. The
//! rules below are [`NodeImageCache`]'s, which the simulator keeps too; this module maps the
//! PSRAM and carries the cache's ops and chunks to and from the host. Each
//! [`HostToBridge::ToNodeChunk`] then names a range of the image, and the P4 builds the record
//! with [`cache_chunk_body`] and queues it as it queues a `ToNode` (`src/bus.rs`), so
//! `Delivered` and `Undelivered` follow unchanged. This is the one record the P4 writes, and its
//! layout is in the node bus's frozen core.
//!
//! **The cache serves only a verified image.** The receiver checks the SHA-256 on `Commit`, and
//! until then every chunk command is refused [`Undelivered::NoImage`]: a corrupt copy would fail
//! every node at once, again and again. A `Begin` for another image starts a new session at once,
//! so a chunk command during an upload is refused rather than sent from a half-written copy. A
//! command whose tag ([`image_tag`]) is not the cached image's is refused too, so a session never
//! sends from an image that replaced its own. `Install` is refused: a cached image only feeds
//! chunks.
//!
//! **The region is the mapped PSRAM**, capped at [`CACHE_MAX`], an app slot. A sector erase needs
//! nothing, and a write is a copy. A P4 whose PSRAM does not map refuses every op as a table with
//! no spare slot does, and every chunk command `NoImage`, and says so once to the first host
//! announced to; everything else runs.
//!
//! The receiver is reset when the host gate closes, as the P4's own update receiver is, so a
//! host that goes away leaves no image for the next one to send from unchecked.
//!
//! [`HostToBridge::CacheImage`]: wartui_proto::link::HostToBridge::CacheImage
//! [`HostToBridge::ToNodeChunk`]: wartui_proto::link::HostToBridge::ToNodeChunk

use esp_hal::peripherals::PSRAM;
use esp_hal::psram::{Psram, PsramConfig};
use static_cell::ConstStaticCell;
use wartui_proto::bus::{Board, DownType, Slot, UPDATE_CHUNK_MAX, UpdateStatus};
use wartui_proto::link::{
    BridgeToHost, NodeImageCache, UPDATE_STATUS_MAX, Undelivered, UpdateStatusBytes,
    cache_chunk_body,
};
use wartui_proto::ota::Receiver;
use wartui_proto::radio_link::update::Op;

use crate::Shared;

/// The most of the PSRAM the cache uses: a node's app slot.
pub const CACHE_MAX: usize = 2 * 1024 * 1024;

/// The P4's log line when its PSRAM did not map.
const NO_PSRAM: &str = "node image cache: no PSRAM";

/// The cache's receiver, with its sector of buffer. Its board is a node's; the cache fills each
/// `Begin` with it, as `src/update.rs` does, so the check passes every image.
static RECEIVER: ConstStaticCell<Receiver> = ConstStaticCell::new(Receiver::new(Board::Esp32c5));

/// The cached image and the receiver that fills it.
pub struct Cache {
    /// Over the mapped PSRAM, up to [`CACHE_MAX`]; empty when none mapped. Its receiver is
    /// static, since it holds a sector of buffer.
    inner: NodeImageCache<&'static mut Receiver, &'static mut [u8]>,
    /// Whether a host has been told there is no PSRAM.
    said: bool,
}

impl Cache {
    /// Map the PSRAM and take it as the cache. Out of line, so the driver stays off `main`'s
    /// frame.
    #[inline(never)]
    pub fn new(psram: PSRAM<'static>) -> Self {
        let region = map(psram);
        let len = region.len().min(CACHE_MAX);
        Self { inner: NodeImageCache::new(RECEIVER.take(), &mut region[..len]), said: false }
    }
}

/// The mapped PSRAM, whole, or nothing when none mapped. Takes the peripheral, so it runs once.
#[allow(unsafe_code)]
fn map(psram: PSRAM<'static>) -> &'static mut [u8] {
    let (addr, size) = Psram::new(psram, PsramConfig::default()).raw_parts();
    if addr.is_null() || size == 0 {
        return &mut [];
    }
    // SAFETY: `Psram::new` mapped `size` bytes at `addr`, and they stay mapped: the driver never
    // unmaps them. Nothing else in this firmware uses that range: no allocator takes it, and the
    // peripheral moved in here, so this is the only slice made over it.
    unsafe { core::slice::from_raw_parts_mut(addr, size) }
}

/// Carry out one `HostToBridge::CacheImage` and answer it.
///
/// Out of line: a decoded op borrows the 1 KB frame.
#[inline(never)]
pub fn take(bytes: &[u8], shared: &Shared) {
    let Some(op) = Op::decode(bytes) else {
        shared.push_error("a cache op did not decode");
        return;
    };
    let status = shared.cache.borrow_mut().inner.take(op);
    if let Some(status) = status {
        answer(status, shared);
    }
}

/// Send the host `status`. Out of line, as each message is, so the two never share a frame.
#[inline(never)]
fn answer(status: UpdateStatus, shared: &Shared) {
    let mut buf = [0u8; UPDATE_STATUS_MAX];
    let len = Op::Status(status).encode(&mut buf);
    let status = UpdateStatusBytes::from_slice(&buf[..len]).unwrap_or_default();
    shared.push_host(&BridgeToHost::CacheImage(status));
}

/// Queue for the node in `slot` the `UpdateChunk` of `len` bytes at `offset` in the cached image
/// tagged `image`, or answer `NoImage` at once.
///
/// Out of line: the record is a kilobyte.
#[inline(never)]
pub fn to_node_chunk(
    shared: &Shared,
    slot: Slot,
    id: u16,
    image: u32,
    session: u8,
    offset: u32,
    len: u16,
) {
    let mut body = [0u8; UPDATE_CHUNK_MAX];
    let built = shared
        .cache
        .borrow()
        .inner
        .image(image)
        .and_then(|bytes| cache_chunk_body(bytes, session, offset, len, &mut body));
    match built {
        Some(n) => crate::bus::to_node(shared, slot, id, DownType::UpdateChunk.as_u8(), &body[..n]),
        None => shared.push_host(&BridgeToHost::Undelivered { id, reason: Undelivered::NoImage }),
    }
}

/// Say there is no PSRAM, once, to the first host present for an announcement.
pub fn say(shared: &Shared) {
    if !shared.host_open() {
        return;
    }
    let mut cache = shared.cache.borrow_mut();
    if cache.said || cache.inner.capacity() > 0 {
        return;
    }
    cache.said = true;
    drop(cache);
    shared.push_log(NO_PSRAM);
}

/// The host gate closed: drop the session and the image it filled.
pub fn host_gone(shared: &Shared) {
    shared.cache.borrow_mut().inner.reset();
}
