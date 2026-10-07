extern crate std;
use std::vec::Vec;

use super::{BULK_DEPTH, ByteSink, Outbox, PRIORITY_DEPTH, USB_PACKET};
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

pub(super) fn log(n: u8) -> BridgeToHost {
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
pub(super) fn received(bytes: &[u8]) -> Vec<BridgeToHost> {
    let mut acc = FrameAccumulator::<{ MAX_FRAME }>::new();
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

pub(super) fn bodies(frames: &[BridgeToHost]) -> Vec<u8> {
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
