use crate::link::{BridgeToHost, MAX_BODY, encode_body};

/// One frame's body: version byte, serialized message and CRC, not yet COBS-encoded.
#[derive(Clone, Copy)]
struct Slot {
    buf: [u8; MAX_BODY],
    len: u16,
}

impl Slot {
    const EMPTY: Self = Self { buf: [0; MAX_BODY], len: 0 };
}

/// A ring of pre-allocated slots, each holding a frame's body.
///
/// Messages are serialized straight into the tail slot rather than built and then
/// moved in, so a frame of over a kilobyte never lands on the stack. COBS is the
/// writer's, a packet at a time (see the module docs).
pub(super) struct Ring<const N: usize> {
    slots: [Slot; N],
    head: usize,
    len: usize,
}

impl<const N: usize> Ring<N> {
    pub(super) const fn new() -> Self {
        // `evict_behind_front` needs a frame behind the front of a full ring.
        const { assert!(N >= 2, "a ring must hold at least two frames") };
        Self { slots: [Slot::EMPTY; N], head: 0, len: 0 }
    }

    pub(super) const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(super) const fn is_full(&self) -> bool {
        self.len == N
    }

    /// Serialize `msg` into the slot past the tail. Fails if the ring is full or
    /// the message does not fit a frame.
    pub(super) fn push(&mut self, msg: &BridgeToHost) -> Result<(), ()> {
        if self.is_full() {
            return Err(());
        }
        let idx = (self.head + self.len) % N;
        let written = encode_body(msg, &mut self.slots[idx].buf).map_err(|_| ())?;
        // MAX_BODY is well under u16::MAX, so the cast cannot lose bits.
        self.slots[idx].len = written as u16;
        self.len += 1;
        Ok(())
    }

    /// The front frame's body. Empty when the ring is.
    pub(super) fn front(&self) -> &[u8] {
        if self.is_empty() {
            return &[];
        }
        let front = &self.slots[self.head];
        &front.buf[..usize::from(front.len)]
    }

    /// Forget every frame.
    pub(super) const fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    pub(super) fn pop(&mut self) {
        if self.len > 0 {
            self.head = (self.head + 1) % N;
            self.len -= 1;
        }
    }

    /// Drop the frame behind the front, keeping the front.
    ///
    /// The front's used bytes move into the slot behind it, which becomes the new
    /// head. The copy is safe while that frame is part-way onto the wire: the
    /// writer's position in the body indexes the same bytes in their new slot.
    pub(super) fn evict_behind_front(&mut self) {
        // Called only on a full ring, and `new` asserts `N >= 2`.
        debug_assert!(self.len >= 2);
        let front = self.head;
        let behind = (front + 1) % N;
        let (src, dst) = if behind > front {
            let (lo, hi) = self.slots.split_at_mut(behind);
            (&lo[front], &mut hi[0])
        } else {
            let (lo, hi) = self.slots.split_at_mut(front);
            (&hi[0], &mut lo[behind])
        };
        let used = src.len as usize;
        dst.buf[..used].copy_from_slice(&src.buf[..used]);
        dst.len = src.len;
        self.head = behind;
        self.len -= 1;
    }
}
