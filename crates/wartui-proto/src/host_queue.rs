//! The P4's queue of link frames bound for the host over high-speed USB.
//!
//! The P4 encodes each frame whole and the USB peripheral takes it in 512-byte packets.
//! Two rings keep a host that falls behind from burying the frames that answer it:
//! priority frames are the ones [`is_priority`] names, and everything else is bulk. Both
//! evict their oldest frame ([`FrameRing`]).
//!
//! Priority wins only at a frame boundary. A bulk frame part-way out is finished first,
//! because the host would otherwise read a priority frame in the middle of it and lose
//! both. A packet may carry the end of one frame and the start of the next, which keeps a
//! quiet link from sending a packet per small frame.
//!
//! # The lone `0x00`
//!
//! A transfer that ends on a full packet is not complete to the host until a short packet
//! follows. The same hold-back that `outbox` documents at 64 bytes happens at 512, so when
//! a packet fills exactly and nothing follows, the next call returns a lone `0x00`: an
//! empty frame every receiver already skips.
//!
//! # A new host
//!
//! The P4 queues nothing while no host holds DTR, and [`HostQueue::clear`] empties both
//! rings when one raises it, so a host never reads a backlog from before it attached.

use crate::frame_ring::FrameRing;
use crate::link::{BridgeToHost, QueueId, QueueStat};

/// The high-speed bulk packet size.
pub const HS_PACKET: usize = 512;

/// Whether a message must survive a backlog.
pub const fn is_priority(msg: &BridgeToHost) -> bool {
    match msg {
        BridgeToHost::Ready { .. }
        | BridgeToHost::SendResult { .. }
        | BridgeToHost::Status { .. }
        | BridgeToHost::Error { .. }
        | BridgeToHost::Update(_) => true,
        BridgeToHost::Rx { .. } | BridgeToHost::Log { .. } | BridgeToHost::Gps { .. } => false,
    }
}

/// Frames waiting to go to the host, `P` bytes of priority and `B` of bulk.
pub struct HostQueue<const P: usize, const B: usize> {
    priority: FrameRing<P>,
    bulk: FrameRing<B>,
    /// The last packet was full, so a short one must follow.
    owes_terminator: bool,
    /// [`HostQueue::clear`] cut a frame off part-way out, so the next packet starts with
    /// a `0x00` that ends the fragment.
    cut: bool,
}

impl<const P: usize, const B: usize> Default for HostQueue<P, B> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const P: usize, const B: usize> HostQueue<P, B> {
    /// An empty queue.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            priority: FrameRing::new(),
            bulk: FrameRing::new(),
            owes_terminator: false,
            cut: false,
        }
    }

    /// Queue a complete COBS frame, terminator included, as `link::encode_frame` writes it.
    ///
    /// Returns `false` when the frame was refused.
    pub fn push(&mut self, frame: &[u8], priority: bool) -> bool {
        if priority { self.priority.push(frame) } else { self.bulk.push(frame) }
    }

    /// Discard everything queued, for a host that has just connected.
    ///
    /// What is queued was meant for nobody, or for a host that has gone. A frame part-way
    /// out is cut off by a `0x00` at the head of the next packet, so the new host's decoder
    /// starts clean. Counts no drop: the drop counts mean congestion while a host reads.
    pub fn clear(&mut self) {
        self.cut |= self.priority.mid_frame() || self.bulk.mid_frame();
        self.priority.clear();
        self.bulk.clear();
    }

    /// Fill `out` with the next bytes for the host and return how many.
    ///
    /// Returns 0 when nothing is queued and no terminator is owed.
    pub fn next_packet(&mut self, out: &mut [u8; HS_PACKET]) -> usize {
        let mut filled = 0;
        if self.cut {
            self.cut = false;
            out[0] = 0x00;
            filled = 1;
        }
        while filled < HS_PACKET {
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
        if filled == 0 && self.owes_terminator {
            self.owes_terminator = false;
            out[0] = 0x00;
            return 1;
        }
        self.owes_terminator = filled == HS_PACKET;
        filled
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
                capacity: B as u32,
                high_water: self.bulk.high_water() as u32,
                dropped: self.bulk.dropped(),
            },
        ]
    }
}
