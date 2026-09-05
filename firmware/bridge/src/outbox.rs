//! Bounded outbound queues for the USB link.
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
//!   they are never dropped to make room.
//! - **bulk** — [`BridgeToHost::Rx`] and [`BridgeToHost::Log`]. A host that has
//!   fallen behind is better served by the newest observations than the oldest,
//!   so the oldest is evicted and counted.
//!
//! The count surfaces in [`BridgeToHost::Status`] as `dropped_tx`. A non-zero
//! value means the host, not the bridge, is the bottleneck.

use wartui_proto::link::{BridgeToHost, MAX_FRAME, encode_frame};

/// Frames held for the host while it is not reading.
///
/// Deep enough to cover a stalled host across several nodes' heartbeat bursts —
/// esp-radio's own receive queue is only ten frames deep and silently discards
/// its oldest, so the useful buffering has to live here.
const BULK_DEPTH: usize = 24;

/// Frames that answer a host request. Short: the host asks for one status at a
/// time, and a backlog here means it is not reading at all.
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
    unflushed: bool,
    dropped: u32,
}

impl Outbox {
    /// The rings are sixteen kilobytes, so on RISC-V this returns through a
    /// hidden out-pointer rather than by value — that is the ABI, not an
    /// optimisation. The only caller passes it to `StaticCell::init_with`, so
    /// the pointer is into `.bss` and nothing touches the stack. Clippy reads
    /// the signature rather than the calling convention and sees two copies of
    /// a large value; there are none.
    #[allow(clippy::large_stack_frames, reason = "returned indirectly, straight into .bss")]
    pub const fn new() -> Self {
        Self {
            priority: Ring::new(),
            bulk: Ring::new(),
            current: None,
            cursor: 0,
            unflushed: false,
            dropped: 0,
        }
    }

    /// Frames discarded rather than delivered, for [`BridgeToHost::Status`].
    pub const fn dropped(&self) -> u32 {
        self.dropped
    }

    /// Queue `msg`, evicting an older bulk frame if that is what it takes.
    ///
    /// Returns whether it was queued. Callers generally ignore the result: the
    /// drop counter is the signal that matters, and there is nowhere better to
    /// report a failure to report something.
    pub fn send(&mut self, msg: &BridgeToHost) -> bool {
        let queued = if is_priority(msg) {
            self.priority.push(msg)
        } else {
            if self.bulk.is_full() {
                self.discard_bulk_front();
            }
            self.bulk.push(msg)
        };

        if queued.is_err() {
            self.dropped = self.dropped.saturating_add(1);
        }
        queued.is_ok()
    }

    /// Evict the oldest bulk frame. If it is the one being written, the write
    /// is abandoned too — the host's frame accumulator resynchronises at the
    /// next terminator, which is exactly what COBS framing is there for.
    fn discard_bulk_front(&mut self) {
        if self.current == Some(Source::Bulk) {
            self.current = None;
            self.cursor = 0;
        }
        self.bulk.pop();
        self.dropped = self.dropped.saturating_add(1);
    }

    /// Write as much as the FIFO will take. Returns whether any byte moved.
    pub fn pump<S: ByteSink>(&mut self, sink: &mut S) -> bool {
        let mut progressed = false;

        loop {
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
                // Frame complete.
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
                    // The endpoint refused the byte outright, so this frame is
                    // already truncated on the wire. Abandon it and let the
                    // host resynchronise rather than emit the rest as garbage.
                    self.finish(source);
                    self.dropped = self.dropped.saturating_add(1);
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
