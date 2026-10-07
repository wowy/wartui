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
//! sends a packet on every `USB_PACKET`th byte by itself, and a flush with
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
//!
//! [`ResetCause::speaks_first`]: crate::link::ResetCause::speaks_first

use crate::link::BridgeToHost;

mod ring;
#[cfg(test)]
mod tests;

use ring::Ring;

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
