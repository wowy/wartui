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
//! Both rings evict rather than refuse their newest: whichever end is dropped the
//! frame is gone, and a host that stopped reading while still sending would
//! otherwise be answered from behind eight stale `Ready` frames.
//!
//! A full ring evicts its oldest frame not yet started. The one part-way onto the
//! wire is finished, because abandoning it wastes the bytes already sent, and a
//! sustained burst keeps the ring full, so it would abandon nearly every frame.
//!
//! A frame the endpoint refuses outright is already truncated on the wire, so a
//! lone `0x00` is written behind it. COBS resynchronises at a terminator and only
//! at a terminator: without one the host would glue the fragment to the whole of
//! the next frame and fail the checksum on both.
//!
//! The same lone `0x00` is what [`Outbox::delimit`] queues at boot. It goes out first
//! when transmit opens, which is the first host frame rather than boot unless the
//! life speaks first ([`ResetCause::speaks_first`]). A reset prints
//! the ROM banner down this endpoint with no `0x00` in it, so without a terminator
//! of our own the host reads banner and first frame as one overlong frame and
//! drops both. A lone `0x00` is an empty frame, which every receiver already
//! skips, so this is not a wire change.
//!
//! The same byte keeps a transfer from ending on a full USB packet. The endpoint
//! sends a packet on every [`USB_PACKET`]th byte by itself, and a flush with
//! nothing left in the FIFO sends nothing, so a pump whose bytes end exactly on a
//! packet boundary ends the transfer with no short packet. Linux `cdc_acm` then
//! holds those bytes in a read that has not completed until the next frame arrives,
//! seconds later on a quiet fleet: 8 of 8 frames of exactly 64 bytes were held so,
//! against none of 1,356 others (`docs/usb-boundary-findings.md`). So a pump that
//! would end on a boundary writes one `0x00` more, and the transfer ends short.
//!
//! The count surfaces in [`BridgeToHost::Status`] as `dropped_tx`, where a non-zero
//! value means frames arrived faster than the host took them: a host not reading,
//! or a burst bigger than the bulk ring plus what USB drains while it arrives.

use crate::link::{BridgeToHost, MAX_FRAME, encode_frame};

/// Frames held for the host while it is not reading.
///
/// Deep enough to cover a stalled host across several nodes' heartbeat bursts —
/// esp-radio's own receive queue is only ten frames deep and silently discards
/// its oldest, so the useful buffering has to live here. What fills it is a node's
/// back-to-back burst: a batch every ~0.5 ms on the air against ~0.8 ms per frame
/// to USB, so the ring holds the difference for as long as the burst lasts.
const BULK_DEPTH: usize = 24;

/// Frames that answer a host request. Short, because the host asks for one at a time.
const PRIORITY_DEPTH: usize = 8;

/// The full-speed bulk max packet size of the USB-Serial-JTAG on both the C5 and the C6.
const USB_PACKET: usize = 64;

/// Somewhere to put bytes that may refuse them.
///
/// Exists so the ring logic below is about queueing rather than about esp-hal.
pub trait ByteSink {
    /// Accept one byte, or report that the FIFO is full.
    fn write_byte(&mut self, byte: u8) -> nb::Result<(), ()>;

    /// Push a partial USB packet out. The hardware sends automatically on every
    /// 64th byte; this is what releases the remainder, and it sends nothing when no
    /// remainder is left.
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
        // `evict_behind_front` needs a frame behind the front of a full ring.
        const { assert!(N >= 2, "a ring must hold at least two frames") };
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

    /// Drop the frame behind the front, keeping the front.
    ///
    /// The front's used bytes move into the slot behind it, which becomes the new
    /// head. The copy is safe while that frame is part-way onto the wire: the
    /// writer's cursor indexes the same bytes in their new slot.
    fn evict_behind_front(&mut self) {
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
    /// A lone terminator is owed ahead of the next frame: to close off a frame
    /// abandoned part-way onto the wire, because [`Outbox::delimit`] asked, or to
    /// end a transfer that stopped on a full packet.
    orphan: bool,
    unflushed: bool,
    /// Bytes written into the current USB packet since one last ended.
    packet_fill: usize,
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
            packet_fill: 0,
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
                self.make_room(Source::Priority);
            }
            self.priority.push(msg)
        } else {
            if self.bulk.is_full() {
                self.make_room(Source::Bulk);
            }
            self.bulk.push(msg)
        };

        if queued.is_err() {
            self.dropped = self.dropped.saturating_add(1);
        }
        queued.is_ok()
    }

    /// Owe the host a lone `0x00` ahead of the next frame.
    ///
    /// Called once at boot, before anything is queued: the ROM banner a reset
    /// prints carries no `0x00`, so it would otherwise run into the first frame
    /// and take it down with it. The `0x00` is sent when transmit opens, which a
    /// life that does not speak first holds until a host has. A frame already
    /// part-way out needs nothing, since its own terminator follows it.
    pub const fn delimit(&mut self) {
        if self.cursor == 0 {
            self.orphan = true;
        }
    }

    /// Evict the oldest frame not yet started from a full ring, to make room for
    /// a newer one. The frame being written is kept and finished.
    fn make_room(&mut self, source: Source) {
        let in_flight = self.current == Some(source);
        match (source, in_flight) {
            (Source::Priority, true) => self.priority.evict_behind_front(),
            (Source::Priority, false) => self.priority.pop(),
            (Source::Bulk, true) => self.bulk.evict_behind_front(),
            (Source::Bulk, false) => self.bulk.pop(),
        }
        self.dropped = self.dropped.saturating_add(1);
    }

    /// Drop the frame being written after the endpoint refused a byte of it.
    ///
    /// The frame is already truncated on the wire, so a terminator is owed to
    /// the host so it can discard the fragment on its own rather than run it
    /// into the next frame.
    fn abandon_front(&mut self, source: Source) {
        self.abandon();
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
                        self.wrote();
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
                    self.wrote();
                    progressed = true;
                }
                Err(nb::Error::WouldBlock) => break,
                Err(nb::Error::Other(())) => {
                    // Refused outright, so this frame is already truncated on the
                    // wire: abandon it and let the host resynchronise.
                    self.abandon_front(source);
                }
            }
        }

        // A transfer that ends on a full packet is not delivered until more bytes
        // follow, so end it with a lone `0x00` instead. Only when nothing follows:
        // the loop leaves `current` empty only once both rings are, and a frame still
        // in flight ends the transfer with its own remaining bytes. That also keeps
        // `orphan` owed only between frames, as `abandon` and `delimit` do, so the
        // zero never lands inside one. An owed orphan is already that byte: one zero
        // both terminates a fragment and starts a packet, so the flag serves both and
        // an orphan still owed needs no second.
        if self.unflushed && self.packet_fill == 0 && self.current.is_none() && !self.orphan {
            match sink.write_byte(0x00) {
                Ok(()) => {
                    self.wrote();
                    progressed = true;
                }
                // The FIFO is full, which is the boundary itself: owe the zero, so it
                // goes out first on the next pump and `is_empty` keeps that pump coming.
                Err(nb::Error::WouldBlock) => self.orphan = true,
                // Refused outright, as for any orphan: there is no zero to be had.
                Err(nb::Error::Other(())) => {}
            }
        }

        if self.unflushed {
            sink.flush();
            self.unflushed = false;
            // A flush ends the packet.
            self.packet_fill = 0;
        }

        progressed
    }

    /// Account for one byte the sink accepted.
    const fn wrote(&mut self) {
        self.unflushed = true;
        self.packet_fill = (self.packet_fill + 1) % USB_PACKET;
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

    use super::{BULK_DEPTH, ByteSink, Outbox, PRIORITY_DEPTH, Ring, USB_PACKET};
    use crate::link::{
        BridgeToHost, Chip, FrameAccumulator, LINK_PROTO_VERSION, LogLevel, LogStr, LoopPhase,
        MAX_FRAME, ResetCause, ShortStr, decode_frame, encode_frame,
    };

    /// A sink with a settable ceiling, so a wedged host can be simulated by
    /// letting exactly `capacity` more bytes through. `refuse_at` makes the
    /// endpoint refuse outright once, when that many bytes have gone out.
    /// `flushes` holds how many bytes had gone out at each flush.
    struct Fake {
        out: Vec<u8>,
        capacity: usize,
        refuse_at: Option<usize>,
        flushes: Vec<usize>,
    }

    impl Fake {
        fn new(capacity: usize) -> Self {
            Self { out: Vec::new(), capacity, refuse_at: None, flushes: Vec::new() }
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
            if self.refuse_at == Some(self.out.len()) {
                self.refuse_at = None;
                return Err(nb::Error::Other(()));
            }
            if self.capacity == 0 {
                return Err(nb::Error::WouldBlock);
            }
            self.capacity -= 1;
            self.out.push(byte);
            Ok(())
        }

        fn flush(&mut self) {
            self.flushes.push(self.out.len());
        }
    }

    fn log(n: u8) -> BridgeToHost {
        let mut message = LogStr::new();
        // One distinct, decodable body per frame, so a test can say which
        // frames survived rather than only how many.
        message.push((b'a' + n) as char).unwrap();
        BridgeToHost::Log { level: LogLevel::Info, message }
    }

    /// A `Log` frame whose encoding is exactly `len` bytes, terminator included, its
    /// message `id` repeated so [`bodies`] can tell frames apart.
    fn log_encoding_to(len: usize, id: u8) -> (BridgeToHost, Vec<u8>) {
        for n in 1..=LogStr::new().capacity() {
            let mut message = LogStr::new();
            for _ in 0..n {
                message.push((b'a' + id) as char).unwrap();
            }
            let msg = BridgeToHost::Log { level: LogLevel::Info, message };
            let mut buf = [0; MAX_FRAME];
            let used = encode_frame(&msg, &mut buf).unwrap();
            if used == len {
                return (msg, buf[..used].to_vec());
            }
        }
        panic!("no log message encodes to {len} bytes");
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
            panel: None,
        }
    }

    fn status(rx_count: u32) -> BridgeToHost {
        BridgeToHost::Status { peer_count: 0, rx_count, dropped_tx: 0, uptime_ms: 0 }
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
                // Decoded as a char: an id past 127 is two bytes of UTF-8.
                BridgeToHost::Log { message, .. } => {
                    message.chars().next().and_then(|c| u8::try_from(c).ok())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn outbox_encodes_and_decodes_frame_when_round_tripped_through_sink() {
        let mut outbox = Outbox::new();
        let mut sink = Fake::open();
        assert!(outbox.send(&log(0)));
        outbox.pump(&mut sink);

        assert_eq!(bodies(&received(&sink.out)), [b'a']);
        assert_eq!(outbox.dropped(), 0);
    }

    #[test]
    fn outbox_writes_priority_frames_before_bulk_frames_when_both_are_queued() {
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
    fn outbox_drops_oldest_bulk_frames_when_bulk_ring_capacity_is_exceeded() {
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
    fn outbox_drops_oldest_priority_frames_when_priority_ring_capacity_is_exceeded() {
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
    fn outbox_keeps_in_flight_bulk_frame_when_bulk_ring_overflows_mid_write() {
        let mut outbox = Outbox::new();
        for n in 0..BULK_DEPTH as u8 {
            outbox.send(&log(n));
        }

        // Get the front frame part-way onto the wire, then stall.
        let mut sink = Fake::new(3);
        outbox.pump(&mut sink);
        assert_eq!(sink.out.len(), 3, "the fake sink should have stalled mid-frame");

        // A new frame evicts the oldest one not yet started.
        outbox.send(&log(BULK_DEPTH as u8));

        sink.capacity = usize::MAX;
        outbox.pump(&mut sink);

        let expected: Vec<u8> =
            core::iter::once(b'a').chain((2..=BULK_DEPTH as u8).map(|n| b'a' + n)).collect();
        assert_eq!(bodies(&received(&sink.out)), expected);
        assert_eq!(outbox.dropped(), 1);
    }

    #[test]
    fn outbox_keeps_in_flight_bulk_frame_when_bulk_ring_overflows_repeatedly_mid_write() {
        let mut outbox = Outbox::new();
        for n in 0..BULK_DEPTH as u8 {
            outbox.send(&log(n));
        }
        let mut sink = Fake::new(3);
        outbox.pump(&mut sink);

        let last = BULK_DEPTH as u8 + 10;
        for n in BULK_DEPTH as u8..last {
            outbox.send(&log(n));
        }

        sink.capacity = usize::MAX;
        outbox.pump(&mut sink);

        // The in-flight frame, then the newest BULK_DEPTH - 1, in order.
        let newest = last - (BULK_DEPTH as u8 - 1);
        let expected: Vec<u8> =
            core::iter::once(b'a').chain((newest..last).map(|n| b'a' + n)).collect();
        assert_eq!(bodies(&received(&sink.out)), expected);
        assert_eq!(outbox.dropped(), 10);
    }

    #[test]
    fn outbox_keeps_in_flight_priority_frame_when_priority_ring_overflows_mid_write() {
        let mut outbox = Outbox::new();
        for n in 0..PRIORITY_DEPTH as u32 {
            outbox.send(&status(n));
        }
        let mut sink = Fake::new(3);
        outbox.pump(&mut sink);
        assert_eq!(sink.out.len(), 3, "the fake sink should have stalled mid-frame");

        outbox.send(&ready());

        sink.capacity = usize::MAX;
        outbox.pump(&mut sink);

        let frames = received(&sink.out);
        let counts: Vec<u32> = frames
            .iter()
            .filter_map(|f| match f {
                BridgeToHost::Status { rx_count, .. } => Some(*rx_count),
                _ => None,
            })
            .collect();
        let expected: Vec<u32> = [0].into_iter().chain(2..PRIORITY_DEPTH as u32).collect();
        assert_eq!(counts, expected);
        assert!(matches!(frames.last(), Some(BridgeToHost::Ready { .. })));
        assert_eq!(frames.len(), PRIORITY_DEPTH);
        assert_eq!(outbox.dropped(), 1);
    }

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

    #[test]
    fn outbox_resynchronises_cobs_stream_when_endpoint_refuses_mid_frame() {
        let mut outbox = Outbox::new();
        for n in 0..3 {
            outbox.send(&log(n));
        }

        let mut sink = Fake::open();
        sink.refuse_at = Some(3);
        outbox.pump(&mut sink);

        // The truncated fragment is closed off on its own and everything behind
        // it decodes; without the terminator the fragment would cost two frames.
        assert_eq!(sink.out[3], 0x00, "the fragment should be terminated");
        assert_eq!(bodies(&received(&sink.out)), [b'b', b'c']);
        assert_eq!(outbox.dropped(), 1);
    }

    #[test]
    fn outbox_writes_lone_terminator_before_next_frame_when_delimited() {
        let mut outbox = Outbox::new();
        outbox.delimit();
        outbox.send(&ready());
        let mut sink = Fake::open();
        outbox.pump(&mut sink);

        assert_eq!(sink.out[0], 0x00, "the delimiter goes first");
        // Behind a banner with no terminator in it, which overflows the host's
        // buffer, the delimiter is what lets `Ready` through.
        let mut stream = std::vec![b'x'; 1500];
        stream.extend_from_slice(&sink.out);
        let frames = received(&stream);
        assert!(matches!(frames[..], [BridgeToHost::Ready { .. }]));
    }

    #[test]
    fn outbox_keeps_in_flight_frame_whole_when_delimited_mid_write() {
        let mut outbox = Outbox::new();
        outbox.send(&log(0));
        let mut sink = Fake::new(3);
        outbox.pump(&mut sink);

        outbox.delimit();
        sink.capacity = usize::MAX;
        outbox.pump(&mut sink);

        assert_eq!(bodies(&received(&sink.out)), [b'a']);
        assert_eq!(sink.out.iter().filter(|&&b| b == 0).count(), 1, "no extra terminator");
    }

    #[test]
    fn outbox_reports_not_empty_when_frame_is_partially_written() {
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
    fn outbox_preserves_queued_frames_without_progress_when_sink_is_wedged() {
        let mut outbox = Outbox::new();
        outbox.send(&log(0));

        let mut wedged = Fake::wedged();
        assert!(!outbox.pump(&mut wedged));
        assert!(wedged.out.is_empty());

        let mut open = Fake::open();
        assert!(outbox.pump(&mut open));
        assert_eq!(bodies(&received(&open.out)), [b'a']);
    }
    #[test]
    fn outbox_pads_transfer_when_frame_ends_on_full_packet() {
        let (msg, frame) = log_encoding_to(USB_PACKET, 0);
        assert_eq!(frame.len(), 64);
        let mut outbox = Outbox::new();
        outbox.send(&msg);
        let mut sink = Fake::open();
        outbox.pump(&mut sink);

        // Ending on the boundary would leave the host's read open until the next
        // frame; the lone `0x00` makes the transfer end on a short packet.
        let mut expected = frame;
        expected.push(0x00);
        assert_eq!(sink.out, expected);
        assert_eq!(sink.flushes, [65]);
        assert_eq!(bodies(&received(&sink.out)), [b'a']);
        assert!(outbox.is_empty());
    }

    #[test]
    fn outbox_writes_no_pad_when_frame_ends_short_of_packet_boundary() {
        for len in [USB_PACKET - 1, USB_PACKET + 1] {
            let (msg, frame) = log_encoding_to(len, 0);
            let mut outbox = Outbox::new();
            outbox.send(&msg);
            let mut sink = Fake::open();
            outbox.pump(&mut sink);

            assert_eq!(sink.out, frame, "{len}-byte frame");
            assert_eq!(bodies(&received(&sink.out)), [b'a']);
        }
    }

    #[test]
    fn outbox_pads_once_at_end_when_two_frames_fill_two_packets() {
        let (first, a) = log_encoding_to(USB_PACKET, 0);
        let (second, b) = log_encoding_to(USB_PACKET, 1);
        let mut outbox = Outbox::new();
        outbox.send(&first);
        outbox.send(&second);
        let mut sink = Fake::open();
        outbox.pump(&mut sink);

        // The boundary between them is not the end of the transfer, so it needs none.
        let mut expected = a;
        expected.extend_from_slice(&b);
        expected.push(0x00);
        assert_eq!(sink.out, expected);
        assert_eq!(sink.flushes, [2 * USB_PACKET + 1]);
        assert_eq!(bodies(&received(&sink.out)), [b'a', b'b']);
    }

    #[test]
    fn outbox_owes_pad_to_next_pump_when_fifo_fills_on_packet_boundary() {
        let (msg, frame) = log_encoding_to(USB_PACKET, 0);
        let mut outbox = Outbox::new();
        outbox.send(&msg);
        let mut sink = Fake::new(USB_PACKET);
        outbox.pump(&mut sink);

        assert_eq!(sink.out, frame, "the full FIFO takes no pad");
        // Still pending, so the bridge keeps pumping rather than idling.
        assert!(!outbox.is_empty());

        sink.capacity = usize::MAX;
        assert!(outbox.pump(&mut sink));
        let mut expected = frame;
        expected.push(0x00);
        assert_eq!(sink.out, expected);
        assert_eq!(sink.flushes, [USB_PACKET, USB_PACKET + 1]);
        assert_eq!(bodies(&received(&sink.out)), [b'a']);
        assert!(outbox.is_empty());
    }

    #[test]
    fn outbox_gives_up_pad_when_endpoint_refuses_it() {
        let (msg, frame) = log_encoding_to(USB_PACKET, 0);
        let mut outbox = Outbox::new();
        outbox.send(&msg);
        let mut sink = Fake::open();
        sink.refuse_at = Some(USB_PACKET);
        outbox.pump(&mut sink);

        assert_eq!(sink.out, frame);
        assert_eq!(sink.flushes, [USB_PACKET]);
        assert!(outbox.is_empty(), "a refused pad is not owed");
    }

    #[test]
    fn outbox_writes_no_pad_when_flush_ends_packet_between_pumps() {
        let (first, a) = log_encoding_to(40, 0);
        let (second, b) = log_encoding_to(USB_PACKET - 40, 1);
        let mut outbox = Outbox::new();
        let mut sink = Fake::open();
        outbox.send(&first);
        outbox.pump(&mut sink);
        outbox.send(&second);
        outbox.pump(&mut sink);

        // 64 bytes in all, but the flush between them sent the first 40 as a short
        // packet of its own, so neither transfer ends on a boundary.
        let mut expected = a;
        expected.extend_from_slice(&b);
        assert_eq!(sink.out, expected);
        assert_eq!(sink.flushes, [40, USB_PACKET]);
        assert_eq!(bodies(&received(&sink.out)), [b'a', b'b']);
    }
    #[test]
    fn outbox_keeps_frame_whole_when_fifo_fills_on_packet_boundary_mid_frame() {
        let (msg, frame) = log_encoding_to(80, 0);
        let mut outbox = Outbox::new();
        outbox.send(&msg);
        let mut sink = Fake::new(USB_PACKET);
        outbox.pump(&mut sink);

        // The frame's last 16 bytes end the transfer, so no zero is owed here.
        sink.capacity = usize::MAX;
        outbox.pump(&mut sink);

        assert_eq!(sink.out, frame, "no 0x00 inside the frame");
        assert_eq!(bodies(&received(&sink.out)), [b'a']);
        assert!(outbox.is_empty());
    }

    #[test]
    fn outbox_delivers_both_frames_when_fifo_fills_on_boundary_between_frames() {
        let (first, _) = log_encoding_to(USB_PACKET, 0);
        let (second, _) = log_encoding_to(30, 1);
        let mut outbox = Outbox::new();
        outbox.send(&first);
        outbox.send(&second);
        let mut sink = Fake::new(USB_PACKET);
        outbox.pump(&mut sink);

        sink.capacity = usize::MAX;
        outbox.pump(&mut sink);

        assert_eq!(bodies(&received(&sink.out)), [b'a', b'b']);
        assert!(outbox.is_empty());
    }
}
