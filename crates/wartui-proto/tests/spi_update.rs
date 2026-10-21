//! The SPI link's update payload, and the P4's sender driven against a C6 built on the
//! node's update receiver.

use sha2::{Digest, Sha256};
use wartui_proto::air::{
    Board, OTA_CHUNK, OtaBeginMsg, OtaChunkMsg, OtaCommitMsg, OtaState, OtaStatusMsg, Version,
};
use wartui_proto::ota::{Receiver, SECTOR, SectorSink};
use wartui_proto::spi::PAYLOAD_MAX;
use wartui_proto::spi::update::{Failure, OP_MAX, Op, REPLY_WAIT_MS, Sender, Stage, TRIES};

const DIGEST: [u8; 32] = [0x42; 32];
/// The session [`Sender`] names for [`DIGEST`].
const SESSION: u8 = 0x42;

/// A slot held in memory, erased to 0xFF.
struct Slot {
    flash: Vec<u8>,
}

impl SectorSink for Slot {
    type Error = ();

    fn capacity(&self) -> u32 {
        self.flash.len() as u32
    }

    fn erase_sector(&mut self, offset: u32) -> Result<(), ()> {
        let at = offset as usize;
        self.flash[at..at + SECTOR].fill(0xFF);
        Ok(())
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), ()> {
        let at = offset as usize;
        self.flash[at..at + data.len()].copy_from_slice(data);
        Ok(())
    }
}

/// The C6's end, as `firmware/p4-radio` drives the receiver.
struct FakeC6 {
    receiver: Box<Receiver>,
    slot: Slot,
    installed: Option<u8>,
}

impl FakeC6 {
    fn new() -> Self {
        Self {
            receiver: Box::new(Receiver::new(Board::Esp32c6)),
            slot: Slot { flash: vec![0xFF; 64 * SECTOR] },
            installed: None,
        }
    }

    fn take(&mut self, payload: &[u8]) -> Option<OtaStatusMsg> {
        match Op::decode(payload).expect("an op") {
            Op::Begin { session, len, sha256 } => {
                let begin = OtaBeginMsg {
                    session,
                    len,
                    sha256,
                    version: Version::new(0, 0, 0),
                    board: Board::Esp32c6,
                };
                Some(self.receiver.begin(&begin, &self.slot))
            }
            Op::Chunk { session, offset, data } => {
                self.receiver.chunk(&OtaChunkMsg { session, offset, data }, &mut self.slot)
            }
            Op::Commit { session } => {
                Some(self.receiver.commit(&OtaCommitMsg { session }, &mut self.slot))
            }
            Op::Install { session } => {
                assert!(self.receiver.is_verified(), "install before verified");
                self.installed = Some(session);
                None
            }
            Op::Status(_) => panic!("a status sent down"),
        }
    }
}

fn image(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn sha256(image: &[u8]) -> [u8; 32] {
    Sha256::digest(image).into()
}

/// What a sent op was, for the tests that follow the sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sent {
    Begin,
    Chunk(u32),
    Commit,
    Install,
}

fn sent(op: &Op<'_>) -> Sent {
    match op {
        Op::Begin { .. } => Sent::Begin,
        Op::Chunk { offset, .. } => Sent::Chunk(*offset),
        Op::Commit { .. } => Sent::Commit,
        Op::Install { .. } => Sent::Install,
        Op::Status(_) => panic!("a status sent down"),
    }
}

/// Runs the sender against `c6` one millisecond per transfer, answering at once, until it
/// stops sending. `lose` drops an op on its way down. Returns what was sent.
fn run(sender: &mut Sender<'_>, c6: &mut FakeC6, mut lose: impl FnMut(Sent) -> bool) -> Vec<Sent> {
    let mut buf = [0u8; OP_MAX];
    let mut log = Vec::new();
    for now in 0..1_000_000 {
        if let Some(len) = sender.next(now, &mut buf) {
            let op = Op::decode(&buf[..len]).expect("an op");
            log.push(sent(&op));
            if !lose(sent(&op))
                && let Some(status) = c6.take(&buf[..len])
            {
                sender.on_status(&status);
            }
        }
        if sender.stage() != Stage::Sending {
            return log;
        }
    }
    panic!("the sender never finished");
}

#[test]
fn spi_update_op_has_frozen_layout_for_each_op() {
    let mut out = [0u8; OP_MAX];
    let mut sha256 = [0u8; 32];
    sha256[0] = 0xAA;
    sha256[31] = 0xBB;
    let begin = Op::Begin { session: 7, len: 0x0001_2345, sha256 };
    let len = begin.encode(&mut out);
    let mut expected = vec![1, 7, 0x45, 0x23, 0x01, 0x00];
    expected.extend_from_slice(&sha256);
    assert_eq!(out[..len], expected[..]);
    assert_eq!(Op::decode(&out[..len]), Some(begin));

    let chunk = Op::Chunk { session: 7, offset: 0x1000, data: &[9, 8, 7] };
    let len = chunk.encode(&mut out);
    assert_eq!(out[..len], [2, 7, 0x00, 0x10, 0x00, 0x00, 9, 8, 7]);
    assert_eq!(Op::decode(&out[..len]), Some(chunk));

    let commit = Op::Commit { session: 7 };
    let len = commit.encode(&mut out);
    assert_eq!(out[..len], [3, 7]);
    assert_eq!(Op::decode(&out[..len]), Some(commit));

    let install = Op::Install { session: 7 };
    let len = install.encode(&mut out);
    assert_eq!(out[..len], [4, 7]);
    assert_eq!(Op::decode(&out[..len]), Some(install));

    let status = Op::Status(OtaStatusMsg {
        session: 7,
        state: OtaState::HashMismatch,
        next_offset: 0x0000_2000,
    });
    let len = status.encode(&mut out);
    assert_eq!(out[..len], [5, 7, 3, 0x00, 0x20, 0x00, 0x00]);
    assert_eq!(Op::decode(&out[..len]), Some(status));
}

#[test]
fn spi_update_op_fits_one_transfer_when_chunk_is_full() {
    const { assert!(OP_MAX <= PAYLOAD_MAX) };
    let data = [0x5A; OTA_CHUNK];
    let mut out = [0u8; OP_MAX];
    let chunk = Op::Chunk { session: 1, offset: 0, data: &data };
    assert_eq!(chunk.encode(&mut out), OP_MAX);
    assert_eq!(Op::decode(&out), Some(chunk));
}

#[test]
fn spi_update_decodes_none_when_op_unknown() {
    for op in [0u8, 6, 0xFF] {
        assert_eq!(Op::decode(&[op, 7, 0, 0, 0, 0, 0]), None);
    }
    assert_eq!(Op::decode(&[]), None);
}

#[test]
fn spi_update_decodes_none_when_payload_short() {
    let mut out = [0u8; OP_MAX];
    let ops = [
        Op::Begin { session: 7, len: 10, sha256: [1; 32] },
        Op::Chunk { session: 7, offset: 0, data: &[] },
        Op::Commit { session: 7 },
        Op::Install { session: 7 },
        Op::Status(OtaStatusMsg { session: 7, state: OtaState::Idle, next_offset: 0 }),
    ];
    for op in ops {
        let len = op.encode(&mut out);
        assert_eq!(Op::decode(&out[..len - 1]), None, "{op:?}");
    }
}

#[test]
fn spi_update_decodes_none_when_chunk_longer_than_ota_chunk() {
    let mut payload = vec![2, 7, 0, 0, 0, 0];
    payload.extend_from_slice(&[0; OTA_CHUNK + 1]);
    assert_eq!(Op::decode(&payload), None);
}

#[test]
fn sender_installs_image_when_c6_verifies_it() {
    // A partial last sector, and a whole number of sectors.
    for len in [3 * SECTOR + 1_500, 4 * SECTOR] {
        let image = image(len);
        let mut sender = Sender::new(&image, sha256(&image), &DIGEST);
        let mut c6 = FakeC6::new();
        let log = run(&mut sender, &mut c6, |_| false);
        assert_eq!(sender.stage(), Stage::Installed);
        assert_eq!(sender.percent(), 100);
        assert_eq!(c6.installed, Some(SESSION));
        assert_eq!(c6.slot.flash[..len], image[..]);
        assert_eq!(log.first(), Some(&Sent::Begin));
        assert_eq!(log[log.len() - 2..], [Sent::Commit, Sent::Install]);
        let chunks = log.iter().filter(|s| matches!(s, Sent::Chunk(_))).count();
        assert_eq!(chunks, len.div_ceil(OTA_CHUNK));
    }
}

#[test]
fn sender_rewinds_to_next_offset_when_status_names_a_gap() {
    let image = image(2 * SECTOR);
    let mut sender = Sender::new(&image, sha256(&image), &DIGEST);
    let mut c6 = FakeC6::new();
    let mut lost = false;
    let log = run(&mut sender, &mut c6, |sent| {
        let lose = !lost && sent == Sent::Chunk(1024);
        lost |= lose;
        lose
    });
    assert_eq!(sender.stage(), Stage::Installed);
    assert_eq!(c6.slot.flash[..image.len()], image[..]);
    // The chunk after the lost one draws a status naming it, and the sender goes back.
    assert_eq!(
        log[..7],
        [
            Sent::Begin,
            Sent::Chunk(0),
            Sent::Chunk(1024),
            Sent::Chunk(2048),
            Sent::Chunk(1024),
            Sent::Chunk(2048),
            Sent::Chunk(3072),
        ]
    );
}

#[test]
fn sender_waits_for_status_when_a_sector_is_sent() {
    let image = image(2 * SECTOR);
    let mut sender = Sender::new(&image, sha256(&image), &DIGEST);
    let mut c6 = FakeC6::new();
    let mut buf = [0u8; OP_MAX];
    let len = sender.next(0, &mut buf).unwrap();
    let status = c6.take(&buf[..len]).unwrap();
    sender.on_status(&status);
    let mut last = None;
    for offset in (0..SECTOR).step_by(OTA_CHUNK) {
        let len = sender.next(1, &mut buf).unwrap();
        assert_eq!(sent(&Op::decode(&buf[..len]).unwrap()), Sent::Chunk(offset as u32));
        last = c6.take(&buf[..len]);
    }
    // Nothing more until the sector's status arrives.
    assert_eq!(sender.next(2, &mut buf), None);
    assert_eq!(sender.next(REPLY_WAIT_MS, &mut buf), None);
    sender.on_status(&last.expect("a status on the sector's flush"));
    assert_eq!(sender.percent(), 50);
    let len = sender.next(REPLY_WAIT_MS, &mut buf).unwrap();
    assert_eq!(sent(&Op::decode(&buf[..len]).unwrap()), Sent::Chunk(SECTOR as u32));
}

#[test]
fn sender_fails_when_c6_reports_hash_mismatch() {
    let image = image(SECTOR + 100);
    let mut sender = Sender::new(&image, [0; 32], &DIGEST);
    let mut c6 = FakeC6::new();
    let log = run(&mut sender, &mut c6, |_| false);
    assert_eq!(sender.stage(), Stage::Failed(Failure::Refused(OtaState::HashMismatch)));
    assert_eq!(log.last(), Some(&Sent::Commit));
    assert_eq!(c6.installed, None);
}

#[test]
fn sender_retries_then_fails_when_no_status_arrives() {
    let image = image(SECTOR);
    let mut sender = Sender::new(&image, sha256(&image), &DIGEST);
    let mut buf = [0u8; OP_MAX];
    let mut begins = 0;
    for now in 0..(u64::from(TRIES) + 2) * REPLY_WAIT_MS {
        if let Some(len) = sender.next(now, &mut buf) {
            assert_eq!(sent(&Op::decode(&buf[..len]).unwrap()), Sent::Begin);
            begins += 1;
        }
    }
    assert_eq!(begins, TRIES);
    assert_eq!(sender.stage(), Stage::Failed(Failure::Silent));
}

#[test]
fn sender_ignores_status_when_session_differs() {
    let image = image(SECTOR);
    let mut sender = Sender::new(&image, sha256(&image), &DIGEST);
    let mut buf = [0u8; OP_MAX];
    sender.next(0, &mut buf).unwrap();
    sender.on_status(&OtaStatusMsg {
        session: SESSION + 1,
        state: OtaState::FlashError,
        next_offset: 0,
    });
    assert_eq!(sender.stage(), Stage::Sending);
}
