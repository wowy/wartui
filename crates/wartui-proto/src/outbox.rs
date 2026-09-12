//! Bounded outbound queues for the bridge's USB link.
//!
//! Only the firmware instantiates this, but it lives here because a `no_std` binary
//! built for `riscv32imac` cannot run a test, and decision logic belongs where
//! `cargo test` reaches it. That is why the other `no_std` modules in this crate are
//! here too, and they point at this paragraph rather than repeating it.
//!
//! `UsbSerialJtag` stops accepting bytes the moment its endpoint FIFO fills,
//! and nothing drains that FIFO unless a host is reading. A blocking write from
//! the receive path would therefore stall the radio for as long as the TUI is
//! wedged or the cable is out — which is the most common way this class of
//! firmware fails in the field, and it fails silently.
//!
//! So the bridge never blocks on the link. Everything it wants to say is
//! encoded into one of two rings and drained a byte at a time by whatever the
//! FIFO will take:
//!
//! - **priority** — [`BridgeToHost::Ready`], [`BridgeToHost::SendResult`],
//!   [`BridgeToHost::Status`] and [`BridgeToHost::Error`]. Each one answers a
//!   question the host asked, or tells it something it cannot re-derive, so
//!   they are served ahead of everything else.
//! - **bulk** — [`BridgeToHost::Rx`] and [`BridgeToHost::Log`]. A host that has
//!   fallen behind is better served by the newest observations than the oldest.
//!
//! Both rings evict their oldest rather than refuse their newest: whichever end is
//! dropped the frame is gone, and a host that stopped reading while still sending
//! would otherwise be answered from behind eight stale `Ready` frames.
//!
//! Evicting a frame already part-way onto the wire leaves a truncated prefix
//! there, so a lone `0x00` is written behind it. COBS resynchronises at a
//! terminator and only at a terminator: without one the host would glue the
//! fragment to the whole of the next frame and fail the checksum on both.
//!
//! The count surfaces in [`BridgeToHost::Status`] as `dropped_tx`, where a non-zero
//! value means the host rather than the bridge is the bottleneck.

use crate::link::{BridgeToHost, MAX_FRAME, encode_frame};

/// Frames held for the host while it is not reading.
///
/// Deep enough to cover a stalled host across several nodes' heartbeat bursts —
/// esp-radio's own receive queue is only ten frames deep and silently discards
/// its oldest, so the useful buffering has to live here.
const BULK_DEPTH: usize = 24;

/// Frames that answer a host request. Short, because the host asks for one at a time.
const PRIORITY_DEPTH: usize = 8;

/// Somewhere to put bytes that may refuse them.
///
/// Exists so the ring logic below is about queueing rather than about esp-hal.
pub trait ByteSink {
    /// Accept one byte, or report that the FIFO is full.
    fn write_byte(&mut self, byte: u8) -> nb::Result<(), ()>;

    /// Push a partial USB packet out. The hardware sends automatically on every
    /// 64th byte; this is what releases the remainder.
    fn flush(&mut self);
}

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
struct Ring<const N: usize> {
    slots: [Slot; N],
    head: usize,
    len: usize,
}

impl<const N: usize> Ring<N> {
    const fn new() -> Self {
        Self { slots: [Slot::EMPTY; N], head: 0, len: 0 }
    }

    const fn is_empty(&self) -> bool {
        self.len == 0
    }

    const fn is_full(&self) -> bool {
        self.len == N
    }

    /// Encode `msg` into the slot past the tail. Fails if the ring is full or
    /// the message does not fit a frame.
    fn push(&mut self, msg: &BridgeToHost) -> Result<(), ()> {
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
    fn byte_at(&self, cursor: usize) -> Option<u8> {
        if self.is_empty() {
            return None;
        }
        let front = &self.slots[self.head];
        if cursor >= front.len as usize { None } else { Some(front.buf[cursor]) }
    }

    fn pop(&mut self) {
        if self.len > 0 {
            self.head = (self.head + 1) % N;
            self.len -= 1;
        }
    }
}

/// Which ring the frame currently being written came from.
///
/// Remembered rather than recomputed: a priority frame arriving mid-transmission
/// must not preempt a bulk frame that is already half on the wire.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Priority,
    Bulk,
}

/// Everything the bridge is waiting to tell the host.
pub struct Outbox {
    priority: Ring<PRIORITY_DEPTH>,
    bulk: Ring<BULK_DEPTH>,
    current: Option<Source>,
    cursor: usize,
    /// A frame was abandoned with bytes already sent, and the terminator that
    /// closes off the fragment has not been accepted by the FIFO yet.
    orphan: bool,
    unflushed: bool,
    dropped: u32,
}

impl Outbox {
    /// The rings are sixteen kilobytes, so on RISC-V this returns through a hidden
    /// out-pointer by ABI rather than by optimisation, and the only caller passes it
    /// to `StaticCell::init_with` — so the pointer is into `.bss`. Clippy reads the
    /// signature rather than the calling convention.
    #[allow(clippy::large_stack_frames, reason = "returned indirectly, straight into .bss")]
    #[allow(
        clippy::new_without_default,
        reason = "a `Default` impl would invite `Outbox::default()`, which builds \
                  sixteen kilobytes somewhere the caller did not choose"
    )]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            priority: Ring::new(),
            bulk: Ring::new(),
            current: None,
            cursor: 0,
            orphan: false,
            unflushed: false,
            dropped: 0,
        }
    }

    /// Frames discarded rather than delivered, for [`BridgeToHost::Status`].
    pub const fn dropped(&self) -> u32 {
        self.dropped
    }

    /// Whether there is anything the host has not been told yet.
    ///
    /// The bridge uses this to tell two silences apart. Nothing queued and
    /// nothing moving is a quiet fleet. Something queued and nothing moving,
    /// while the host is still sending commands, is a transmit path that has
    /// stopped draining — which is not a state this end can talk its way out
    /// of, since talking is the part that is broken.
    pub const fn is_empty(&self) -> bool {
        self.priority.is_empty() && self.bulk.is_empty() && self.current.is_none() && !self.orphan
    }

    /// Queue `msg`, evicting an older bulk frame if that is what it takes.
    ///
    /// Returns whether it was queued. Callers generally ignore the result: the
    /// drop counter is the signal that matters, and there is nowhere better to
    /// report a failure to report something.
    pub fn send(&mut self, msg: &BridgeToHost) -> bool {
        let queued = if is_priority(msg) {
            if self.priority.is_full() {
                self.discard_front(Source::Priority);
            }
            self.priority.push(msg)
        } else {
            if self.bulk.is_full() {
                self.discard_front(Source::Bulk);
            }
            self.bulk.push(msg)
        };

        if queued.is_err() {
            self.dropped = self.dropped.saturating_add(1);
        }
        queued.is_ok()
    }

    /// Evict the oldest frame from a ring to make room for a newer one.
    ///
    /// If it is the frame being written, the write is abandoned and a
    /// terminator is owed to the host so it can discard the fragment on its
    /// own rather than run it into the next frame.
    fn discard_front(&mut self, source: Source) {
        if self.current == Some(source) {
            self.abandon();
        }
        match source {
            Source::Priority => self.priority.pop(),
            Source::Bulk => self.bulk.pop(),
        }
        self.dropped = self.dropped.saturating_add(1);
    }

    /// Give up on the frame being written. Owes a terminator if any of it has
    /// already gone out; owes nothing if it had not started.
    fn abandon(&mut self) {
        if self.cursor > 0 {
            self.orphan = true;
        }
        self.current = None;
        self.cursor = 0;
    }

    /// Write as much as the FIFO will take. Returns whether any byte moved.
    pub fn pump<S: ByteSink>(&mut self, sink: &mut S) -> bool {
        let mut progressed = false;

        loop {
            // Close off an abandoned fragment before anything else, or the
            // host would read it as the head of the frame that follows.
            if self.orphan {
                match sink.write_byte(0x00) {
                    Ok(()) => {
                        self.orphan = false;
                        self.unflushed = true;
                        progressed = true;
                    }
                    Err(nb::Error::WouldBlock) => break,
                    // The endpoint is refusing bytes outright, so there is no
                    // terminator to be had. Give up on it rather than spin.
                    Err(nb::Error::Other(())) => self.orphan = false,
                }
            }

            let source = match self.current {
                Some(source) => source,
                None => match self.next_source() {
                    Some(source) => {
                        self.current = Some(source);
                        self.cursor = 0;
                        source
                    }
                    None => break,
                },
            };

            let byte = match source {
                Source::Priority => self.priority.byte_at(self.cursor),
                Source::Bulk => self.bulk.byte_at(self.cursor),
            };

            let Some(byte) = byte else {
                self.finish(source);
                continue;
            };

            match sink.write_byte(byte) {
                Ok(()) => {
                    self.cursor += 1;
                    self.unflushed = true;
                    progressed = true;
                }
                Err(nb::Error::WouldBlock) => break,
                Err(nb::Error::Other(())) => {
                    // Refused outright, so this frame is already truncated on the
                    // wire: abandon it and let the host resynchronise.
                    self.discard_front(source);
                }
            }
        }

        if self.unflushed {
            sink.flush();
            self.unflushed = false;
        }

        progressed
    }

    fn next_source(&self) -> Option<Source> {
        if !self.priority.is_empty() {
            Some(Source::Priority)
        } else if !self.bulk.is_empty() {
            Some(Source::Bulk)
        } else {
            None
        }
    }

    fn finish(&mut self, source: Source) {
        match source {
            Source::Priority => self.priority.pop(),
            Source::Bulk => self.bulk.pop(),
        }
        self.current = None;
        self.cursor = 0;
    }
}

/// Whether a message must survive a backlog.
const fn is_priority(msg: &BridgeToHost) -> bool {
    match msg {
        BridgeToHost::Ready { .. }
        | BridgeToHost::SendResult { .. }
        | BridgeToHost::Status { .. }
        | BridgeToHost::Error { .. } => true,
        BridgeToHost::Rx { .. } | BridgeToHost::Log { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec::Vec;

    use super::{BULK_DEPTH, ByteSink, Outbox, PRIORITY_DEPTH};
    use crate::link::{
        BridgeToHost, Chip, FrameAccumulator, LINK_PROTO_VERSION, LogLevel, LogStr, LoopPhase,
        ResetCause, ShortStr, decode_frame,
    };

    /// A sink with a settable ceiling, so a wedged host can be simulated by
    /// letting exactly `capacity` more bytes through.
    struct Fake {
        out: Vec<u8>,
        capacity: usize,
    }

    impl Fake {
        fn new(capacity: usize) -> Self {
            Self { out: Vec::new(), capacity }
        }

        fn wedged() -> Self {
            Self::new(0)
        }

        fn open() -> Self {
            Self::new(usize::MAX)
        }
    }

    impl ByteSink for Fake {
        fn write_byte(&mut self, byte: u8) -> nb::Result<(), ()> {
            if self.capacity == 0 {
                return Err(nb::Error::WouldBlock);
            }
            self.capacity -= 1;
            self.out.push(byte);
            Ok(())
        }

        fn flush(&mut self) {}
    }

    fn log(n: u8) -> BridgeToHost {
        let mut message = LogStr::new();
        // One distinct, decodable body per frame, so a test can say which
        // frames survived rather than only how many.
        message.push((b'a' + n) as char).unwrap();
        BridgeToHost::Log { level: LogLevel::Info, message }
    }

    fn ready() -> BridgeToHost {
        BridgeToHost::Ready {
            chip: Chip::Esp32C6,
            mac: [1, 2, 3, 4, 5, 6],
            fw_version: ShortStr::new(),
            proto_version: LINK_PROTO_VERSION,
            reset_cause: ResetCause::PowerOn,
            last_phase: LoopPhase::Unknown,
            heap_free: 0,
            uptime_ms: 0,
        }
    }

    fn status(rx_count: u32) -> BridgeToHost {
        BridgeToHost::Status { channel: 6, peer_count: 0, rx_count, dropped_tx: 0, uptime_ms: 0 }
    }

    /// Every complete frame the host would recover from these bytes.
    fn received(bytes: &[u8]) -> Vec<BridgeToHost> {
        let mut acc = FrameAccumulator::<{ super::MAX_FRAME }>::new();
        let mut out = Vec::new();
        for &byte in bytes {
            if let Some(frame) = acc.push(byte)
                && let Ok(msg) = decode_frame(frame)
            {
                out.push(msg);
            }
        }
        out
    }

    fn bodies(frames: &[BridgeToHost]) -> Vec<u8> {
        frames
            .iter()
            .filter_map(|f| match f {
                BridgeToHost::Log { message, .. } => Some(message.as_bytes()[0]),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_frame_survives_the_round_trip() {
        let mut outbox = Outbox::new();
        let mut sink = Fake::open();
        assert!(outbox.send(&log(0)));
        outbox.pump(&mut sink);

        assert_eq!(bodies(&received(&sink.out)), [b'a']);
        assert_eq!(outbox.dropped(), 0);
    }

    #[test]
    fn priority_is_written_before_bulk() {
        let mut outbox = Outbox::new();
        let mut sink = Fake::open();
        outbox.send(&log(0));
        outbox.send(&ready());
        outbox.pump(&mut sink);

        let frames = received(&sink.out);
        assert!(matches!(frames[0], BridgeToHost::Ready { .. }));
        assert!(matches!(frames[1], BridgeToHost::Log { .. }));
    }

    #[test]
    fn a_full_bulk_ring_keeps_the_newest() {
        let mut outbox = Outbox::new();
        for n in 0..BULK_DEPTH as u8 + 4 {
            outbox.send(&log(n));
        }
        let mut sink = Fake::open();
        outbox.pump(&mut sink);

        // The four oldest were evicted; the newest BULK_DEPTH remain, in order.
        let expected: Vec<u8> = (4..BULK_DEPTH as u8 + 4).map(|n| b'a' + n).collect();
        assert_eq!(bodies(&received(&sink.out)), expected);
        assert_eq!(outbox.dropped(), 4);
    }

    #[test]
    fn a_full_priority_ring_keeps_the_newest() {
        let mut outbox = Outbox::new();
        // A host that has stopped reading but goes on sending `Identify` gets
        // answered every time; the answer that finally matters is the last.
        for _ in 0..PRIORITY_DEPTH {
            outbox.send(&ready());
        }
        outbox.send(&status(99));

        let mut sink = Fake::open();
        outbox.pump(&mut sink);

        let frames = received(&sink.out);
        assert!(
            frames.iter().any(|f| matches!(f, BridgeToHost::Status { rx_count: 99, .. })),
            "the newest priority frame was dropped in favour of a stale one"
        );
        assert_eq!(frames.len(), PRIORITY_DEPTH);
    }

    #[test]
    fn a_frame_evicted_mid_write_does_not_take_the_next_one_with_it() {
        let mut outbox = Outbox::new();
        for n in 0..BULK_DEPTH as u8 {
            outbox.send(&log(n));
        }

        // Get the front frame part-way onto the wire, then stall.
        let mut sink = Fake::new(3);
        outbox.pump(&mut sink);
        assert_eq!(sink.out.len(), 3, "the fake sink should have stalled mid-frame");

        // A new frame evicts the one being written.
        outbox.send(&log(BULK_DEPTH as u8));

        sink.capacity = usize::MAX;
        outbox.pump(&mut sink);

        // The truncated head is discarded on its own and everything behind it
        // decodes; without the terminator the fragment would cost two frames.
        let expected: Vec<u8> = (1..=BULK_DEPTH as u8).map(|n| b'a' + n).collect();
        assert_eq!(bodies(&received(&sink.out)), expected);
    }

    #[test]
    fn is_empty_tracks_a_frame_all_the_way_off_the_wire() {
        // The bridge resets itself when this says there is something to send and
        // nothing has moved for three seconds, so a half-written frame must count as
        // pending: reporting empty between `send` and the last byte makes a wedged
        // endpoint look like a quiet one.
        let mut outbox = Outbox::new();
        assert!(outbox.is_empty(), "a fresh outbox has nothing to say");

        outbox.send(&ready());
        assert!(!outbox.is_empty(), "queued but not yet written");

        // One byte through a sink that will take no more: the frame is now
        // half on the wire, which is the case the naive check gets wrong.
        let mut trickle = Fake::new(1);
        outbox.pump(&mut trickle);
        assert!(!outbox.is_empty(), "part written is still pending");

        let mut open = Fake::open();
        outbox.pump(&mut open);
        assert!(outbox.is_empty(), "fully written is finally empty");
    }

    #[test]
    fn a_wedged_sink_never_reports_progress_or_loses_a_queued_frame() {
        let mut outbox = Outbox::new();
        outbox.send(&log(0));

        let mut wedged = Fake::wedged();
        assert!(!outbox.pump(&mut wedged));
        assert!(wedged.out.is_empty());

        let mut open = Fake::open();
        assert!(outbox.pump(&mut open));
        assert_eq!(bodies(&received(&open.out)), [b'a']);
    }
}
