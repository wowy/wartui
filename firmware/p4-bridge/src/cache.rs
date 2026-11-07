//! The node image cache: one node image in PSRAM, which the P4 builds every node's
//! `UpdateChunk` from.
//!
//! Every node on a board takes the same image, so `wartui ota` sends it over the host link once,
//! as [`HostToBridge::CacheImage`] ops in the `wartui_proto::radio_link::update` layout. The P4
//! drives a [`Receiver`] over the PSRAM with them, as it does over flash for its own image
//! (`src/update.rs`), and answers each status as a priority [`BridgeToHost::CacheImage`]. Each
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
use wartui_proto::bus::{Board, DownType, Slot, UPDATE_CHUNK_MAX, UpdateState, UpdateStatus};
use wartui_proto::link::{
    BridgeToHost, UPDATE_STATUS_MAX, Undelivered, UpdateStatusBytes, cache_chunk_body, image_tag,
};
use wartui_proto::ota::{Receiver, SECTOR, SectorSink};
use wartui_proto::radio_link::update::{Op, Received, receive};

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
    /// The mapped PSRAM, up to [`CACHE_MAX`]; empty when none mapped.
    region: &'static mut [u8],
    /// Static, since it holds a sector of buffer.
    receiver: &'static mut Receiver,
    /// The [`image_tag`] of the receiver's session, from the `Begin` it accepted.
    tag: Option<u32>,
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
        Self { region: &mut region[..len], receiver: RECEIVER.take(), tag: None, said: false }
    }

    /// Hand `op` to the receiver, returning the status to send.
    fn take(&mut self, op: Op<'_>) -> Option<UpdateStatus> {
        let begin = match op {
            Op::Begin { session, sha256, .. } => Some((session, image_tag(&sha256))),
            _ => None,
        };
        let mut sink = Sink(&mut *self.region);
        let slot = (!sink.0.is_empty()).then_some(&mut sink);
        match receive(&mut *self.receiver, op, slot, false) {
            Received::Status(status) => {
                // An accepted `Begin` is the receiver's session now. A refused one leaves the
                // session, and so its tag, as they were.
                if let (Some((session, tag)), Some(status)) = (begin, status)
                    && status.session == session
                    && matches!(status.state, UpdateState::Receiving | UpdateState::Verified)
                {
                    self.tag = Some(tag);
                }
                status
            }
            Received::Install(session) => {
                Some(UpdateStatus { session, state: UpdateState::FlashError, next_offset: 0 })
            }
        }
    }

    /// The verified image tagged `tag`, if the cache holds it.
    fn image(&self, tag: u32) -> Option<&[u8]> {
        if !self.receiver.is_verified() || self.tag != Some(tag) {
            return None;
        }
        let len = usize::try_from(self.receiver.total_len()?).ok()?;
        self.region.get(..len)
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

/// The cache's region as a receiver's slot.
struct Sink<'a>(&'a mut [u8]);

/// A write or erase past the region's end, which the receiver's length check rules out.
struct OutOfRange;

impl SectorSink for Sink<'_> {
    type Error = OutOfRange;

    fn capacity(&self) -> u32 {
        u32::try_from(self.0.len()).unwrap_or(u32::MAX)
    }

    fn erase_sector(&mut self, offset: u32) -> Result<(), OutOfRange> {
        self.range(offset, SECTOR).map(|_| ())
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), OutOfRange> {
        self.range(offset, data.len())?.copy_from_slice(data);
        Ok(())
    }
}

impl Sink<'_> {
    fn range(&mut self, offset: u32, len: usize) -> Result<&mut [u8], OutOfRange> {
        let start = usize::try_from(offset).map_err(|_| OutOfRange)?;
        let end = start.checked_add(len).ok_or(OutOfRange)?;
        self.0.get_mut(start..end).ok_or(OutOfRange)
    }
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
    let status = shared.cache.borrow_mut().take(op);
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
        .image(image)
        .and_then(|bytes| cache_chunk_body(bytes, session, offset, len, &mut body));
    match built {
        Some(n) => crate::bus::to_node(shared, slot, id, DownType::UpdateChunk.as_u8(), &body[..n]),
        None => shared.push_host(&BridgeToHost::Undelivered { id, reason: Undelivered::NoImage }),
    }
}

/// Say there is no PSRAM, once, to the first host present for an announcement.
pub fn say(shared: &Shared) {
    if !shared.presence.is_open() {
        return;
    }
    let mut cache = shared.cache.borrow_mut();
    if cache.said || !cache.region.is_empty() {
        return;
    }
    cache.said = true;
    drop(cache);
    shared.push_log(NO_PSRAM);
}

/// The host gate closed: drop the session and the image it filled.
pub fn host_gone(shared: &Shared) {
    let mut cache = shared.cache.borrow_mut();
    cache.receiver.reset();
    cache.tag = None;
}
