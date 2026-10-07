use crate::link::{BridgeToHost, MAX_FRAME, encode_frame};

/// One encoded frame, terminator included.
#[derive(Clone, Copy)]
struct Slot {
    buf: [u8; MAX_FRAME],
    len: u16,
}

impl Slot {
    const EMPTY: Self = Self { buf: [0; MAX_FRAME], len: 0 };
}

/// A ring of pre-allocated slots.
///
/// Messages are encoded straight into the tail slot rather than built and then
/// moved in, so a half-kilobyte frame never lands on the stack.
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

    /// Encode `msg` into the slot past the tail. Fails if the ring is full or
    /// the message does not fit a frame.
    pub(super) fn push(&mut self, msg: &BridgeToHost) -> Result<(), ()> {
        if self.is_full() {
            return Err(());
        }
        let idx = (self.head + self.len) % N;
        let written = encode_frame(msg, &mut self.slots[idx].buf).map_err(|_| ())?;
        // MAX_FRAME is 512, so the cast cannot lose bits.
        self.slots[idx].len = written as u16;
        self.len += 1;
        Ok(())
    }

    /// The byte at `cursor` of the front frame, or `None` once it is exhausted.
    ///
    /// Returning a byte rather than a slice keeps the writer from holding a
    /// borrow on the ring across its own bookkeeping.
    pub(super) fn byte_at(&self, cursor: usize) -> Option<u8> {
        if self.is_empty() {
            return None;
        }
        let front = &self.slots[self.head];
        if cursor >= front.len as usize { None } else { Some(front.buf[cursor]) }
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
    /// writer's cursor indexes the same bytes in their new slot.
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

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec::Vec;

    use super::Ring;
    use crate::outbox::tests::{bodies, log, received};

    #[test]
    fn ring_keeps_front_bytes_when_evicting_behind_front_across_wrap() {
        let mut ring = Ring::<3>::new();
        for n in 0..3 {
            ring.push(&log(n)).unwrap();
        }
        ring.pop();
        ring.pop();
        ring.push(&log(3)).unwrap();
        ring.push(&log(4)).unwrap();
        // The front, log(2), sits in the last slot; the one behind it wraps to 0.
        let front: Vec<u8> = (0..).map_while(|i| ring.byte_at(i)).collect();

        ring.evict_behind_front();

        let kept: Vec<u8> = (0..).map_while(|i| ring.byte_at(i)).collect();
        assert_eq!(kept, front);
        assert_eq!(bodies(&received(&kept)), [b'c']);
        ring.pop();
        let next: Vec<u8> = (0..).map_while(|i| ring.byte_at(i)).collect();
        assert_eq!(bodies(&received(&next)), [b'e']);
    }
}
