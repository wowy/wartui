//! The node's update receiver, driven against a slot held in memory.

use sha2::{Digest, Sha256};
use wartui_proto::air::{
    Board, OTA_CHUNK, OtaAbortMsg, OtaBeginMsg, OtaChunkMsg, OtaCommitMsg, OtaState, OtaStatusMsg,
    Version,
};
use wartui_proto::ota::{Receiver, SECTOR, SectorSink};

const BOARD: Board = Board::Esp32c6;
const CAPACITY: u32 = 64 * SECTOR as u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Erase(u32),
    Write(u32, usize),
}

/// A slot that behaves like NOR flash: erase sets a sector to 0xFF, and a write lands only
/// on erased bytes. It records every operation and fails every one once told to.
struct FakeSink {
    capacity: u32,
    flash: Vec<u8>,
    ops: Vec<Op>,
    fail: bool,
}

impl FakeSink {
    fn new() -> Self {
        Self::with_capacity(CAPACITY)
    }

    /// A slot of `capacity` bytes, backed to the end of its last sector so an erase past
    /// the slot shows as a failed assertion rather than a panic in the fake.
    fn with_capacity(capacity: u32) -> Self {
        let backing = (capacity as usize).next_multiple_of(SECTOR);
        Self { capacity, flash: vec![0; backing], ops: Vec::new(), fail: false }
    }

    fn erases(&self) -> Vec<u32> {
        self.ops
            .iter()
            .filter_map(|op| match op {
                Op::Erase(at) => Some(*at),
                Op::Write(..) => None,
            })
            .collect()
    }
}

impl SectorSink for FakeSink {
    type Error = ();

    fn capacity(&self) -> u32 {
        self.capacity
    }

    fn erase_sector(&mut self, offset: u32) -> Result<(), ()> {
        assert_eq!(offset as usize % SECTOR, 0, "erase at a sector boundary");
        assert!(offset + SECTOR as u32 <= self.capacity, "erase within the slot");
        if self.fail {
            return Err(());
        }
        self.ops.push(Op::Erase(offset));
        self.flash[offset as usize..offset as usize + SECTOR].fill(0xFF);
        Ok(())
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), ()> {
        assert_eq!(offset % 4, 0, "write offset aligned to 4");
        assert_eq!(data.len() % 4, 0, "write length aligned to 4");
        if self.fail {
            return Err(());
        }
        let range = offset as usize..offset as usize + data.len();
        assert!(self.flash[range.clone()].iter().all(|&b| b == 0xFF), "write onto erased flash");
        self.ops.push(Op::Write(offset, data.len()));
        self.flash[range].copy_from_slice(data);
        Ok(())
    }
}

fn image(len: usize) -> Vec<u8> {
    // Never 0xFF, so a byte left erased does not pass for image.
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn begin_for(session: u8, image: &[u8]) -> OtaBeginMsg {
    OtaBeginMsg {
        session,
        len: image.len() as u32,
        sha256: Sha256::digest(image).into(),
        version: Version::new(0, 2, 0),
        board: BOARD,
    }
}

fn chunk_at(session: u8, image: &[u8], offset: usize) -> OtaChunkMsg<'_> {
    let end = (offset + OTA_CHUNK).min(image.len());
    OtaChunkMsg { session, offset: offset as u32, data: &image[offset..end] }
}

fn status(session: u8, state: OtaState, next_offset: u32) -> OtaStatusMsg {
    OtaStatusMsg { session, state, next_offset }
}

/// Send every chunk in order, returning what each one reported.
fn send_all(
    rx: &mut Receiver,
    sink: &mut FakeSink,
    session: u8,
    image: &[u8],
) -> Vec<Option<OtaStatusMsg>> {
    (0..image.len())
        .step_by(OTA_CHUNK)
        .map(|offset| rx.chunk(&chunk_at(session, image, offset), sink))
        .collect()
}

fn receiver() -> Box<Receiver> {
    Box::new(Receiver::new(BOARD))
}

#[test]
fn receiver_verifies_image_when_length_is_not_a_multiple_of_sector_or_chunk() {
    let image = image(3 * SECTOR + 2 * OTA_CHUNK + 7);
    let (mut rx, mut sink) = (receiver(), FakeSink::new());
    let begin = begin_for(1, &image);
    assert_eq!(rx.begin(&begin, &sink), status(1, OtaState::Receiving, 0));
    send_all(&mut rx, &mut sink, 1, &image);
    let len = image.len() as u32;
    assert_eq!(
        rx.commit(&OtaCommitMsg { session: 1 }, &mut sink),
        status(1, OtaState::Verified, len)
    );
    assert!(rx.is_verified());

    // The image, then 0xFF padding to the next multiple of 4, then erased flash.
    assert_eq!(&sink.flash[..image.len()], image.as_slice());
    let padded = image.len().next_multiple_of(4);
    assert!(sink.flash[image.len()..padded].iter().all(|&b| b == 0xFF));
    assert!(sink.flash[padded..4 * SECTOR].iter().all(|&b| b == 0xFF));
    assert_eq!(sink.ops.last(), Some(&Op::Write(3 * SECTOR as u32, padded - 3 * SECTOR)));
}

#[test]
fn receiver_verifies_image_when_length_is_a_whole_number_of_sectors() {
    let image = image(4 * SECTOR);
    let (mut rx, mut sink) = (receiver(), FakeSink::new());
    rx.begin(&begin_for(1, &image), &sink);
    send_all(&mut rx, &mut sink, 1, &image);
    let ops_before_commit = sink.ops.len();
    assert_eq!(
        rx.commit(&OtaCommitMsg { session: 1 }, &mut sink),
        status(1, OtaState::Verified, image.len() as u32)
    );
    assert_eq!(sink.ops.len(), ops_before_commit, "nothing left to write at commit");
    assert_eq!(&sink.flash[..image.len()], image.as_slice());
}

#[test]
fn receiver_erases_each_sector_once_when_before_its_first_write() {
    let image = image(5 * SECTOR + 100);
    let (mut rx, mut sink) = (receiver(), FakeSink::new());
    rx.begin(&begin_for(1, &image), &sink);
    send_all(&mut rx, &mut sink, 1, &image);
    rx.commit(&OtaCommitMsg { session: 1 }, &mut sink);

    let sectors: Vec<u32> = (0..6).map(|i| i * SECTOR as u32).collect();
    assert_eq!(sink.erases(), sectors, "one erase per sector, none beyond the image");
    for (i, pair) in sink.ops.chunks(2).enumerate() {
        let at = sectors[i];
        assert!(matches!(pair, [Op::Erase(e), Op::Write(w, _)] if *e == at && *w == at));
    }
}

#[test]
fn receiver_reports_status_when_sector_flushes_and_not_mid_sector() {
    let image = image(2 * SECTOR + OTA_CHUNK);
    let (mut rx, mut sink) = (receiver(), FakeSink::new());
    rx.begin(&begin_for(3, &image), &sink);
    let reports = send_all(&mut rx, &mut sink, 3, &image);
    let at = |n: usize| Some(status(3, OtaState::Receiving, (n * SECTOR) as u32));
    assert_eq!(reports, vec![None, None, None, at(1), None, None, None, at(2), None]);
}

#[test]
fn receiver_ignores_duplicate_chunk_when_offset_is_behind_and_reports_status() {
    let image = image(3 * OTA_CHUNK);
    let (mut rx, mut sink) = (receiver(), FakeSink::new());
    rx.begin(&begin_for(1, &image), &sink);
    send_all(&mut rx, &mut sink, 1, &image[..2 * OTA_CHUNK]);
    let next = 2 * OTA_CHUNK as u32;
    assert_eq!(
        rx.chunk(&chunk_at(1, &image, 0), &mut sink),
        Some(status(1, OtaState::Receiving, next))
    );
    assert_eq!(rx.status().next_offset, next);
    assert!(sink.ops.is_empty());
}

#[test]
fn receiver_reports_next_offset_when_chunk_leaves_a_gap_and_recovers_from_it() {
    let image = image(SECTOR + 10);
    let (mut rx, mut sink) = (receiver(), FakeSink::new());
    rx.begin(&begin_for(1, &image), &sink);
    assert_eq!(rx.chunk(&chunk_at(1, &image, 0), &mut sink), None);
    let gap = rx.chunk(&chunk_at(1, &image, 2 * OTA_CHUNK), &mut sink);
    assert_eq!(gap, Some(status(1, OtaState::Receiving, OTA_CHUNK as u32)));

    // Resend from where the node said, through to the end.
    for offset in (OTA_CHUNK..image.len()).step_by(OTA_CHUNK) {
        rx.chunk(&chunk_at(1, &image, offset), &mut sink);
    }
    assert_eq!(rx.commit(&OtaCommitMsg { session: 1 }, &mut sink).state, OtaState::Verified);
    assert_eq!(&sink.flash[..image.len()], image.as_slice());
}

#[test]
fn receiver_refuses_chunk_when_offset_is_misaligned() {
    let image = image(3 * OTA_CHUNK);
    let (mut rx, mut sink) = (receiver(), FakeSink::new());
    rx.begin(&begin_for(1, &image), &sink);
    let misaligned = OtaChunkMsg { session: 1, offset: 4, data: &image[4..4 + OTA_CHUNK] };
    assert_eq!(rx.chunk(&misaligned, &mut sink), Some(status(1, OtaState::Receiving, 0)));
    assert_eq!(rx.status().next_offset, 0);
}

#[test]
fn receiver_refuses_chunk_when_short_and_not_final() {
    let image = image(3 * OTA_CHUNK);
    let (mut rx, mut sink) = (receiver(), FakeSink::new());
    rx.begin(&begin_for(1, &image), &sink);
    let short = OtaChunkMsg { session: 1, offset: 0, data: &image[..OTA_CHUNK - 4] };
    assert_eq!(rx.chunk(&short, &mut sink), Some(status(1, OtaState::Receiving, 0)));
    assert_eq!(rx.status().next_offset, 0);
}

#[test]
fn receiver_refuses_chunk_when_it_runs_past_the_image() {
    let image = image(OTA_CHUNK + 10);
    let longer = self::image(2 * OTA_CHUNK);
    let (mut rx, mut sink) = (receiver(), FakeSink::new());
    rx.begin(&begin_for(1, &image), &sink);
    rx.chunk(&chunk_at(1, &image, 0), &mut sink);
    let past = chunk_at(1, &longer, OTA_CHUNK);
    assert_eq!(rx.chunk(&past, &mut sink), Some(status(1, OtaState::Receiving, OTA_CHUNK as u32)));
}

#[test]
fn receiver_reports_missing_offset_when_commit_arrives_early() {
    let image = image(SECTOR + OTA_CHUNK);
    let (mut rx, mut sink) = (receiver(), FakeSink::new());
    rx.begin(&begin_for(1, &image), &sink);
    send_all(&mut rx, &mut sink, 1, &image[..2 * OTA_CHUNK]);
    let ops = sink.ops.len();
    assert_eq!(
        rx.commit(&OtaCommitMsg { session: 1 }, &mut sink),
        status(1, OtaState::Receiving, 2 * OTA_CHUNK as u32)
    );
    assert_eq!(sink.ops.len(), ops, "nothing flushed before the image is whole");
}

#[test]
fn receiver_reports_hash_mismatch_when_image_differs_from_its_hash() {
    let image = image(SECTOR + 3);
    let (mut rx, mut sink) = (receiver(), FakeSink::new());
    let mut begin = begin_for(1, &image);
    begin.sha256[0] ^= 1;
    rx.begin(&begin, &sink);
    send_all(&mut rx, &mut sink, 1, &image);
    let len = image.len() as u32;
    assert_eq!(
        rx.commit(&OtaCommitMsg { session: 1 }, &mut sink),
        status(1, OtaState::HashMismatch, len)
    );
    assert!(!rx.is_verified());
    assert_eq!(
        rx.commit(&OtaCommitMsg { session: 1 }, &mut sink),
        status(1, OtaState::HashMismatch, len)
    );
}

#[test]
fn receiver_refuses_begin_when_board_differs() {
    let image = image(100);
    let (mut rx, sink) = (receiver(), FakeSink::new());
    let mut begin = begin_for(1, &image);
    begin.board = Board::Esp32c5;
    assert_eq!(rx.begin(&begin, &sink), status(1, OtaState::WrongBoard, 0));
    assert!(!rx.is_active());
}

#[test]
fn receiver_refuses_begin_when_image_is_empty_or_larger_than_slot() {
    let (mut rx, sink) = (receiver(), FakeSink::new());
    let mut begin = begin_for(1, &[]);
    assert_eq!(rx.begin(&begin, &sink), status(1, OtaState::TooLarge, 0));
    begin.len = CAPACITY + 1;
    assert_eq!(rx.begin(&begin, &sink), status(1, OtaState::TooLarge, 0));
    assert!(!rx.is_active());
    begin.len = CAPACITY;
    assert_eq!(rx.begin(&begin, &sink), status(1, OtaState::Receiving, 0));
}

#[test]
fn receiver_keeps_session_when_begin_is_refused() {
    // A refused begin is another image, and leaves the one in progress alone.
    let image = image(2 * OTA_CHUNK);
    let (mut rx, mut sink) = (receiver(), FakeSink::new());
    rx.begin(&begin_for(1, &image), &sink);
    rx.chunk(&chunk_at(1, &image, 0), &mut sink);
    let mut wrong = begin_for(2, &image);
    wrong.board = Board::Esp32c5;
    rx.begin(&wrong, &sink);
    assert_eq!(rx.status(), status(1, OtaState::Receiving, OTA_CHUNK as u32));
}

#[test]
fn receiver_reports_idle_when_chunk_or_commit_names_another_session() {
    let image = image(2 * OTA_CHUNK);
    let (mut rx, mut sink) = (receiver(), FakeSink::new());
    rx.begin(&begin_for(1, &image), &sink);
    assert_eq!(rx.chunk(&chunk_at(2, &image, 0), &mut sink), Some(status(2, OtaState::Idle, 0)));
    assert_eq!(rx.commit(&OtaCommitMsg { session: 2 }, &mut sink), status(2, OtaState::Idle, 0));
    assert_eq!(rx.status(), status(1, OtaState::Receiving, 0), "session 1 untouched");
}

#[test]
fn receiver_reports_idle_when_chunk_or_commit_arrives_with_no_session() {
    // A node that rebooted mid-transfer: the host re-sends its begin on seeing this.
    let image = image(2 * OTA_CHUNK);
    let (mut rx, mut sink) = (receiver(), FakeSink::new());
    assert_eq!(rx.chunk(&chunk_at(4, &image, 0), &mut sink), Some(status(4, OtaState::Idle, 0)));
    assert_eq!(rx.commit(&OtaCommitMsg { session: 4 }, &mut sink), status(4, OtaState::Idle, 0));
    assert_eq!(rx.status(), status(0, OtaState::Idle, 0));
    assert!(sink.ops.is_empty());
}

#[test]
fn receiver_keeps_progress_when_begin_repeats_identically() {
    let image = image(2 * SECTOR);
    let (mut rx, mut sink) = (receiver(), FakeSink::new());
    let begin = begin_for(1, &image);
    rx.begin(&begin, &sink);
    send_all(&mut rx, &mut sink, 1, &image[..SECTOR + OTA_CHUNK]);
    let progress = status(1, OtaState::Receiving, (SECTOR + OTA_CHUNK) as u32);
    assert_eq!(rx.begin(&begin, &sink), progress);

    // And the hash carried on with it.
    for offset in (SECTOR + OTA_CHUNK..image.len()).step_by(OTA_CHUNK) {
        rx.chunk(&chunk_at(1, &image, offset), &mut sink);
    }
    assert_eq!(rx.commit(&OtaCommitMsg { session: 1 }, &mut sink).state, OtaState::Verified);
}

#[test]
fn receiver_restarts_when_begin_differs() {
    let image = image(2 * SECTOR);
    let (mut rx, mut sink) = (receiver(), FakeSink::new());
    let begin = begin_for(1, &image);
    rx.begin(&begin, &sink);
    send_all(&mut rx, &mut sink, 1, &image[..SECTOR]);

    let mut other_version = begin;
    other_version.version = Version::new(0, 2, 1);
    assert_eq!(rx.begin(&other_version, &sink), status(1, OtaState::Receiving, 0));

    send_all(&mut rx, &mut sink, 1, &image[..SECTOR]);
    assert_eq!(rx.begin(&begin_for(2, &image), &sink), status(2, OtaState::Receiving, 0));
    send_all(&mut rx, &mut sink, 2, &image);
    assert_eq!(rx.commit(&OtaCommitMsg { session: 2 }, &mut sink).state, OtaState::Verified);
}

#[test]
fn receiver_holds_flash_error_when_messages_follow_until_a_begin() {
    let image = image(2 * SECTOR + 8);
    let (mut rx, mut sink) = (receiver(), FakeSink::new());
    let begin = begin_for(1, &image);
    rx.begin(&begin, &sink);
    sink.fail = true;
    let reports = send_all(&mut rx, &mut sink, 1, &image[..SECTOR]);
    let failed = status(1, OtaState::FlashError, SECTOR as u32);
    assert_eq!(reports.last(), Some(&Some(failed)));

    sink.fail = false;
    assert_eq!(rx.chunk(&chunk_at(1, &image, SECTOR), &mut sink), Some(failed));
    assert_eq!(rx.commit(&OtaCommitMsg { session: 1 }, &mut sink), failed);
    assert!(sink.ops.is_empty());

    // An identical begin retries the failed session from the start.
    assert_eq!(rx.begin(&begin, &sink), status(1, OtaState::Receiving, 0));
    send_all(&mut rx, &mut sink, 1, &image);
    assert_eq!(rx.commit(&OtaCommitMsg { session: 1 }, &mut sink).state, OtaState::Verified);
}

#[test]
fn receiver_restarts_when_identical_begin_follows_hash_mismatch() {
    // A host that restarted and reused its session byte, now sending the right image.
    let image = image(SECTOR + 3);
    let (mut rx, mut sink) = (receiver(), FakeSink::new());
    let begin = begin_for(1, &image);
    let mut corrupt = image.clone();
    corrupt[0] ^= 1;
    rx.begin(&begin, &sink);
    send_all(&mut rx, &mut sink, 1, &corrupt);
    assert_eq!(rx.commit(&OtaCommitMsg { session: 1 }, &mut sink).state, OtaState::HashMismatch);

    assert_eq!(rx.begin(&begin, &sink), status(1, OtaState::Receiving, 0));
    send_all(&mut rx, &mut sink, 1, &image);
    assert_eq!(rx.commit(&OtaCommitMsg { session: 1 }, &mut sink).state, OtaState::Verified);
    assert_eq!(&sink.flash[..image.len()], image.as_slice());
}

#[test]
fn receiver_keeps_verified_when_identical_begin_repeats() {
    let image = image(OTA_CHUNK + 5);
    let (mut rx, mut sink) = (receiver(), FakeSink::new());
    let begin = begin_for(1, &image);
    rx.begin(&begin, &sink);
    send_all(&mut rx, &mut sink, 1, &image);
    let verified = rx.commit(&OtaCommitMsg { session: 1 }, &mut sink);
    assert_eq!(verified.state, OtaState::Verified);
    assert_eq!(rx.begin(&begin, &sink), verified);
    assert!(rx.is_verified());
}

#[test]
fn receiver_refuses_begin_when_image_reaches_past_slot_last_whole_sector() {
    // A slot 100 bytes past a sector boundary: an image there would need an erase past it.
    let capacity = 3 * SECTOR as u32 + 100;
    let (mut rx, mut sink) = (receiver(), FakeSink::with_capacity(capacity));
    let past = image(3 * SECTOR + 1);
    assert_eq!(rx.begin(&begin_for(1, &past), &sink), status(1, OtaState::TooLarge, 0));

    let fits = image(3 * SECTOR);
    assert_eq!(rx.begin(&begin_for(2, &fits), &sink), status(2, OtaState::Receiving, 0));
    send_all(&mut rx, &mut sink, 2, &fits);
    assert_eq!(rx.commit(&OtaCommitMsg { session: 2 }, &mut sink).state, OtaState::Verified);
}

#[test]
fn receiver_reports_flash_error_when_final_sector_write_fails() {
    let image = image(OTA_CHUNK + 1);
    let (mut rx, mut sink) = (receiver(), FakeSink::new());
    rx.begin(&begin_for(1, &image), &sink);
    send_all(&mut rx, &mut sink, 1, &image);
    sink.fail = true;
    let len = image.len() as u32;
    assert_eq!(
        rx.commit(&OtaCommitMsg { session: 1 }, &mut sink),
        status(1, OtaState::FlashError, len)
    );
}

#[test]
fn receiver_drops_session_when_abort_names_it() {
    let image = image(OTA_CHUNK);
    let (mut rx, sink) = (receiver(), FakeSink::new());
    rx.begin(&begin_for(1, &image), &sink);
    assert_eq!(rx.abort(&OtaAbortMsg { session: 1 }), status(1, OtaState::Idle, 0));
    assert!(!rx.is_active());
}

#[test]
fn receiver_keeps_session_when_abort_names_another() {
    let image = image(OTA_CHUNK);
    let (mut rx, sink) = (receiver(), FakeSink::new());
    rx.begin(&begin_for(1, &image), &sink);
    assert_eq!(rx.abort(&OtaAbortMsg { session: 2 }), status(2, OtaState::Idle, 0));
    assert_eq!(rx.status(), status(1, OtaState::Receiving, 0));
}

#[test]
fn receiver_drops_session_when_reset() {
    let image = image(OTA_CHUNK);
    let (mut rx, sink) = (receiver(), FakeSink::new());
    rx.begin(&begin_for(1, &image), &sink);
    rx.reset();
    assert!(!rx.is_active());
    assert_eq!(rx.total_len(), None);
}

#[test]
fn receiver_reports_verified_again_when_commit_repeats() {
    let image = image(SECTOR + 1);
    let (mut rx, mut sink) = (receiver(), FakeSink::new());
    rx.begin(&begin_for(1, &image), &sink);
    send_all(&mut rx, &mut sink, 1, &image);
    let verified = rx.commit(&OtaCommitMsg { session: 1 }, &mut sink);
    let ops = sink.ops.len();
    assert_eq!(verified.state, OtaState::Verified);
    assert_eq!(rx.commit(&OtaCommitMsg { session: 1 }, &mut sink), verified);
    assert_eq!(sink.ops.len(), ops, "nothing written twice");
    assert_eq!(rx.chunk(&chunk_at(1, &image, 0), &mut sink), Some(verified));
}

#[test]
fn receiver_exposes_session_when_one_is_open() {
    let image = image(3000);
    let (mut rx, sink) = (receiver(), FakeSink::new());
    assert_eq!(rx.board(), BOARD);
    assert!(!rx.is_active());
    rx.begin(&begin_for(1, &image), &sink);
    assert!(rx.is_active());
    assert_eq!(rx.total_len(), Some(3000));
    assert_eq!(rx.version(), Some(Version::new(0, 2, 0)));
}
