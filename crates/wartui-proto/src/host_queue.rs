//! The P4's queue of link frames bound for the host over its UART.
//!
//! The P4 encodes each frame whole, and the UART's DMA takes them in writes of up to
//! [`HOST_CHUNK`] bytes. Two rings keep a host that falls behind from burying the frames that
//! answer it:
//! priority frames are the ones [`is_priority`] names, and everything else is bulk. Both
//! evict their oldest frame ([`FrameRing`]).
//!
//! Priority wins only at a frame boundary. A bulk frame part-way out is finished first,
//! because the host would otherwise read a priority frame in the middle of it and lose
//! both. A chunk may carry the end of one frame and the start of the next, so a busy link
//! fills each write.
//!
//! The bulk ring's bytes are an array ([`HostQueue`]) or a slice handed over at boot
//! ([`HostQueueIn::with_bulk`]), as the P4 hands it PSRAM.
//!
//! The queue also carries a phone's WardriveESP text lines ([`crate::wardrive_esp`]), one a
//! record, cut with `\n` rather than `0x00` ([`HostQueueIn::set_terminator`]), since a phone
//! splits its stream on `\n`. The P4 sets the terminator at each attachment, before it clears the
//! queue, so the byte that ends a cut record is the new host's. [`HostQueueIn::push_bulk_within`]
//! holds that stream's bulk under a bound of its own.
//!
//! # A new host
//!
//! The P4 queues nothing while no host is present, and [`HostQueueIn::clear`] empties both
//! rings when one arrives, so a host never reads a backlog from before it attached. A record
//! part-way out is cut off with the terminator, so the new host's reader starts clean.

use crate::frame_ring::{FrameRing, FrameRingIn};
use crate::link::{BridgeToHost, QueueId, QueueStat};

/// The most bytes one write to the host carries: the P4's transmit DMA buffer.
pub const HOST_CHUNK: usize = 4096;

/// Whether a message must survive a backlog.
pub const fn is_priority(msg: &BridgeToHost) -> bool {
    match msg {
        BridgeToHost::Ready { .. }
        | BridgeToHost::Delivered { .. }
        | BridgeToHost::Undelivered { .. }
        | BridgeToHost::Status { .. }
        | BridgeToHost::Error { .. }
        | BridgeToHost::Update(_)
        | BridgeToHost::CacheImage(_)
        | BridgeToHost::PowerCycled { .. }
        | BridgeToHost::Fleet(_) => true,
        BridgeToHost::FromNode { .. } | BridgeToHost::Log { .. } | BridgeToHost::Gps { .. } => {
            false
        }
    }
}

/// Frames waiting to go to the host, `P` bytes of priority and the bytes of `S` of bulk.
pub struct HostQueueIn<const P: usize, S> {
    priority: FrameRing<P>,
    bulk: FrameRingIn<S>,
    /// [`HostQueueIn::clear`] cut a frame off part-way out, so the next chunk starts with
    /// the terminator that ends the fragment.
    cut: bool,
    /// The byte that ends a cut record: `0x00` for COBS frames.
    terminator: u8,
}

/// Frames waiting to go to the host, `P` bytes of priority and `B` of bulk, in arrays.
pub type HostQueue<const P: usize, const B: usize> = HostQueueIn<P, [u8; B]>;

impl<const P: usize, const B: usize> Default for HostQueue<P, B> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const P: usize, const B: usize> HostQueue<P, B> {
    /// An empty queue.
    #[must_use]
    pub const fn new() -> Self {
        Self::with_bulk([0; B])
    }
}

impl<const P: usize, S> HostQueueIn<P, S> {
    /// An empty queue whose bulk ring is `bulk`.
    #[must_use]
    pub const fn with_bulk(bulk: S) -> Self {
        Self {
            priority: FrameRing::new(),
            bulk: FrameRingIn::over(bulk),
            cut: false,
            terminator: 0x00,
        }
    }

    /// Set the byte [`HostQueueIn::fill`] writes after [`HostQueueIn::clear`] cut a record
    /// part-way out: `0x00` unless set, `\n` for text lines.
    pub const fn set_terminator(&mut self, byte: u8) {
        self.terminator = byte;
    }
}

impl<const P: usize, S: AsRef<[u8]> + AsMut<[u8]>> HostQueueIn<P, S> {
    /// Move the bulk ring onto `bulk`, empty, for a firmware that has its bytes only at boot.
    /// Whatever the old ring held is gone, uncounted.
    pub fn set_bulk(&mut self, bulk: S) {
        self.bulk = FrameRingIn::over(bulk);
    }

    /// Queue a complete COBS frame, terminator included, as `link::encode_frame` writes it.
    ///
    /// Returns `false` when the frame was refused.
    pub fn push(&mut self, frame: &[u8], priority: bool) -> bool {
        if priority { self.priority.push(frame) } else { self.bulk.push(frame) }
    }

    /// Queue `frame` as bulk, evicting the oldest bulk frames until the bulk ring holds at most
    /// `limit` bytes ([`FrameRingIn::push_within`]).
    ///
    /// Returns `false` when the frame was refused.
    pub fn push_bulk_within(&mut self, frame: &[u8], limit: usize) -> bool {
        self.bulk.push_within(frame, limit)
    }

    /// Discard everything queued, for a host that has just connected.
    ///
    /// What is queued was meant for nobody, or for a host that has gone. A frame part-way
    /// out is cut off by the terminator at the head of the next chunk, so the new host's
    /// decoder starts clean. Counts no drop: the drop counts mean congestion while a host reads.
    pub fn clear(&mut self) {
        self.cut |= self.priority.mid_frame() || self.bulk.mid_frame();
        self.priority.clear();
        self.bulk.clear();
    }

    /// Fill `out` with the next bytes for the host and return how many.
    ///
    /// Returns 0 when nothing is queued.
    pub fn next_chunk(&mut self, out: &mut [u8; HOST_CHUNK]) -> usize {
        self.fill(out)
    }

    /// Fill `out` with the next bytes for the host, as [`HostQueueIn::next_chunk`] does a chunk,
    /// and return how many.
    ///
    /// Returns 0 when nothing is queued or `out` is empty.
    pub fn fill(&mut self, out: &mut [u8]) -> usize {
        let mut filled = 0;
        if out.is_empty() {
            return 0;
        }
        if self.cut {
            self.cut = false;
            out[0] = self.terminator;
            filled = 1;
        }
        while filled < out.len() {
            // Priority only at a boundary: a frame part-way out is finished first.
            let from_priority = if self.priority.mid_frame() {
                true
            } else if self.bulk.mid_frame() {
                false
            } else if !self.priority.is_empty() {
                true
            } else if !self.bulk.is_empty() {
                false
            } else {
                break;
            };
            filled += if from_priority {
                self.priority.read(&mut out[filled..]).0
            } else {
                self.bulk.read(&mut out[filled..]).0
            };
        }
        filled
    }

    /// How many bytes the bulk ring holds: 0 over an empty slice, which refuses every bulk frame.
    #[must_use]
    pub fn bulk_capacity(&self) -> usize {
        self.bulk.capacity()
    }

    /// Both rings' depth, deepest point and drops, priority first.
    #[must_use]
    pub fn stats(&self) -> [QueueStat; 2] {
        [
            QueueStat {
                queue: QueueId::HostPriority,
                capacity: P as u32,
                high_water: self.priority.high_water() as u32,
                dropped: self.priority.dropped(),
            },
            QueueStat {
                queue: QueueId::HostBulk,
                capacity: self.bulk.capacity() as u32,
                high_water: self.bulk.high_water() as u32,
                dropped: self.bulk.dropped(),
            },
        ]
    }
}
