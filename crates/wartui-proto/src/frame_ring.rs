//! A byte ring of whole frames, for queues that must drop a frame and never half of one.
//!
//! The P4 holds frames bound for the host and the C6 frames bound for the air, each in a
//! buffer sized at build time. Both want the newest data when a consumer falls behind, so
//! a full ring evicts its oldest frame. Each frame is stored behind a `u16` length so
//! eviction and reading always land on a frame boundary.
//!
//! A frame the reader has started is never evicted: the reader has put part of it on the
//! wire, and discarding the rest truncates it. Eviction then takes the frame behind it,
//! sliding the in-progress frame forward over the gap. Sliding costs one copy of a frame
//! the size of one packet or two, against a second cursor into the middle of the ring.
//!
//! Every eviction and every refused frame counts one drop. [`FrameRing::high_water`] is
//! the deepest the ring has been, in bytes, to size it against a measured burst.

/// Bytes of length prefix ahead of each frame.
const PREFIX: usize = 2;

/// Up to `N` bytes of frames, each behind a two-byte length.
pub struct FrameRing<const N: usize> {
    buf: [u8; N],
    /// Where the oldest frame's prefix starts.
    head: usize,
    /// Bytes in use, prefixes included.
    len: usize,
    /// Whether the oldest frame has been partly read.
    reading: bool,
    /// How much of the oldest frame has been read.
    read_off: usize,
    high_water: usize,
    dropped: u32,
}

impl<const N: usize> Default for FrameRing<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> FrameRing<N> {
    /// Total bytes, length prefixes included.
    pub const CAPACITY: usize = N;

    /// An empty ring.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buf: [0; N],
            head: 0,
            len: 0,
            reading: false,
            read_off: 0,
            high_water: 0,
            dropped: 0,
        }
    }

    /// Queue `frame`, evicting the oldest whole frames until it fits.
    ///
    /// Returns `false`, and counts a drop, when it cannot fit even in an empty ring or the
    /// frame being read blocks it. Each frame evicted counts one drop as well.
    pub fn push(&mut self, frame: &[u8]) -> bool {
        let need = PREFIX + frame.len();
        let pinned = if self.reading { self.head_size() } else { 0 };
        if frame.len() > usize::from(u16::MAX) || need > N - pinned {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        while N - self.len < need {
            if self.reading {
                self.evict_behind_head();
            } else {
                self.pop_head();
            }
            self.dropped = self.dropped.saturating_add(1);
        }
        let at = self.wrap(self.head + self.len);
        self.write(at, &(frame.len() as u16).to_le_bytes());
        self.write(self.wrap(at + PREFIX), frame);
        self.len += need;
        self.high_water = self.high_water.max(self.len);
        true
    }

    /// Copy the next bytes of the current frame into `out`, starting the next frame when
    /// none is in progress.
    ///
    /// Returns how many bytes were copied and whether that finished the frame.
    pub fn read(&mut self, out: &mut [u8]) -> (usize, bool) {
        if self.len == 0 {
            return (0, false);
        }
        if !self.reading {
            self.reading = true;
            self.read_off = 0;
        }
        let frame_len = self.head_len();
        let n = out.len().min(frame_len - self.read_off);
        let from = self.wrap(self.head + PREFIX + self.read_off);
        for (i, byte) in out[..n].iter_mut().enumerate() {
            *byte = self.buf[self.wrap(from + i)];
        }
        self.read_off += n;
        let finished = self.read_off == frame_len;
        if finished {
            self.pop_head();
        }
        (n, finished)
    }

    /// Whether a frame has been partly read.
    #[must_use]
    pub const fn mid_frame(&self) -> bool {
        self.reading
    }

    /// Whether no frame is queued.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bytes queued, prefixes included.
    #[must_use]
    pub const fn len_bytes(&self) -> usize {
        self.len
    }

    /// The most bytes queued at once.
    #[must_use]
    pub const fn high_water(&self) -> usize {
        self.high_water
    }

    /// Frames lost to eviction or refusal.
    #[must_use]
    pub const fn dropped(&self) -> u32 {
        self.dropped
    }

    const fn wrap(&self, index: usize) -> usize {
        index % N
    }

    fn write(&mut self, at: usize, bytes: &[u8]) {
        for (i, &byte) in bytes.iter().enumerate() {
            let slot = self.wrap(at + i);
            self.buf[slot] = byte;
        }
    }

    /// The oldest frame's body length.
    fn head_len(&self) -> usize {
        usize::from(u16::from_le_bytes([self.buf[self.head], self.buf[self.wrap(self.head + 1)]]))
    }

    /// The oldest frame's size with its prefix.
    fn head_size(&self) -> usize {
        PREFIX + self.head_len()
    }

    fn pop_head(&mut self) {
        let size = self.head_size();
        self.head = self.wrap(self.head + size);
        self.len -= size;
        self.reading = false;
        self.read_off = 0;
    }

    /// Drop the frame behind the one being read, if there is one, by sliding the one being
    /// read forward over it.
    fn evict_behind_head(&mut self) {
        let size = self.head_size();
        if self.len == size {
            return;
        }
        let behind = self.wrap(self.head + size);
        let gap = PREFIX
            + usize::from(u16::from_le_bytes([self.buf[behind], self.buf[self.wrap(behind + 1)]]));
        // Back to front, since the ranges overlap whenever the gap is shorter than the frame.
        for i in (0..size).rev() {
            let from = self.wrap(self.head + i);
            let to = self.wrap(self.head + gap + i);
            self.buf[to] = self.buf[from];
        }
        self.head = self.wrap(self.head + gap);
        self.len -= gap;
    }
}
