//! What the C6 takes of each [`Down::HostStart`](super::Down::HostStart) and
//! [`Down::HostBytes`](super::Down::HostBytes): the credit that resynchronizes.
//!
//! The P4 holds its bytes in flight, `sent.wrapping_sub(accounted)`, under
//! [`HOST_WINDOW_BYTES`](super::HOST_WINDOW_BYTES), and the C6's [`Up::Credit`](super::Up::Credit)
//! is what brings it back down. A packet that fails its CRC on the way to the C6 takes its bytes
//! with it. Were the C6 to credit only the bytes it received, those would stay in flight for
//! good, and each loss would shrink the window until the stream stalls
//! (`docs/c6-ble-host-findings.md`, "A lost P4 → C6 packet is not recovered").
//!
//! So each `HostBytes` carries its stream offset, and the C6 accounts for the gap the next offset
//! reveals as though it had sent it. Nothing is resent, which would need the P4 to keep a copy of
//! every byte in flight, since its host queue hands each byte on once. The gap costs the central
//! the frames it cut, which fail its CRC, and the C6 writes one `0x00` into its notify ring so
//! the central's COBS reader resynchronizes at once instead of merging the next frame into the
//! torn one.
//!
//! The UART carries packets in order, so an offset behind the expected one is a byte the C6 has
//! already taken, and a [`Down::HostStart`](super::Down::HostStart) ahead of it means the bytes
//! between were lost.
//!
//! Offsets compare by `at.wrapping_sub(expected)` read as `i32`, so the counter wraps freely.
//! Two offsets more than 2 GiB apart read the wrong way round, which a 16 KiB window never
//! reaches.

/// What the C6 takes of a `Down::HostBytes`.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accept {
    /// Take the bytes from `skip` on.
    Take {
        /// Leading bytes the C6 has already taken.
        skip: usize,
        /// Bytes lost on the link ahead of these. The C6 adds them to its `accounted` total and,
        /// when nonzero, writes one `0x00` into its notify ring ahead of the bytes.
        gap: u32,
    },
    /// The C6 has taken every byte already. Take nothing.
    Duplicate,
}

/// The next stream offset the C6 expects from the P4.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Inflow {
    /// `None` until the first `HostStart` or `HostBytes` names one.
    expected: Option<u32>,
}

impl Inflow {
    /// Expecting nothing yet: the first offset seen becomes the base, with no gap. The C6 starts
    /// here at boot, and the P4 resynchronizes its `sent` counter when the C6 starts a new life.
    #[must_use]
    pub const fn new() -> Self {
        Self { expected: None }
    }

    /// A central took the host queue, and its first byte is at `at`. Returns the bytes before
    /// `at` that have not arrived: the UART is ordered, so they never will, and the C6 adds them
    /// to its `accounted` total. Bytes before `at` that arrive later are an earlier
    /// attachment's, and [`Inflow::accept`] reads them as [`Accept::Duplicate`].
    #[must_use]
    pub fn start(&mut self, at: u32) -> u32 {
        let lost = match self.expected {
            Some(expected) if ahead(at, expected) => at.wrapping_sub(expected),
            _ => 0,
        };
        self.expected = Some(at);
        lost
    }

    /// What to take of `len` bytes at stream offset `at`.
    pub fn accept(&mut self, at: u32, len: usize) -> Accept {
        let end = at.wrapping_add(len as u32);
        let Some(expected) = self.expected else {
            self.expected = Some(end);
            return Accept::Take { skip: 0, gap: 0 };
        };
        if ahead(at, expected) || at == expected {
            self.expected = Some(end);
            return Accept::Take { skip: 0, gap: at.wrapping_sub(expected) };
        }
        let behind = expected.wrapping_sub(at) as usize;
        if behind >= len {
            return Accept::Duplicate;
        }
        self.expected = Some(end);
        Accept::Take { skip: behind, gap: 0 }
    }
}

/// Whether `at` is past `expected`, across the wrap.
const fn ahead(at: u32, expected: u32) -> bool {
    (at.wrapping_sub(expected) as i32) > 0
}
