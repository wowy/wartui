//! The records of the P4 bridge's diagnostics log on its microSD card.
//!
//! A log file is a run of records, each COBS-encoded and ended by a zero:
//!
//! ```text
//! COBS( kind:u8 || uptime_ms:u32le || payload || crc32:u32le ) || 0x00
//! ```
//!
//! The CRC is [`crc32`], over the kind, the uptime and the payload: a log file outlives the build
//! that wrote it, as a flash record does, so it takes the CRC every tool can check
//! ([`crate::crc`]). A `Frame` or `Snapshot` payload keeps the link's own CRC-16 inside it, as
//! encoded. A record holds no
//! zero byte before its terminator, so a reader resynchronizes at the next zero after a torn block
//! or a record that fails its CRC, and the zeros a cut-off write leaves read as empty records
//! ([`records`]). A bad record costs that record alone.
//!
//! | kind | record           | payload                                                         |
//! | ---- | ---------------- | --------------------------------------------------------------- |
//! | 1    | [`Opened`]       | postcard; first in every file                                   |
//! | 2    | `Frame`          | a [`BridgeToHost`] link frame, without its terminator           |
//! | 3    | `Snapshot`       | the same, for a `Status` the log built itself                   |
//! | 4    | `PhoneLine`      | the [`Class`], 0 answer or 1 stream, then the line's bytes      |
//! | 5    | `PhoneCommand`   | the bytes a phone wrote                                         |
//! | 6    | `Owner`          | postcard `(Input, Holder before, Holder after)`                 |
//! | 7    | `Dropped`        | `u32le`: records the log lost since the last `Dropped`          |
//! | 8    | `Closed`         | a [`CloseReason`] byte                                          |
//!
//! **A frame is stored as the link encoded it.** The bridge already encodes every frame it sends,
//! so logging one is a copy, and the reader decodes it with the host's own [`decode_frame`]: a
//! message the host reads, the log reads, with no second encoding to keep in step. A node's
//! heartbeat or panic record is kept so too, as a plain `Frame` of the `FromNode` the bridge
//! forwarded ([`logged`]).
//!
//! **Uptime is the P4's, in 32 bits.** The P4 knows no date, and a `u32` of milliseconds wraps
//! after 49 days. [`Opened`] carries the full uptime once per file, and [`Clock`] unwraps the
//! records after it.

use core::ops::Range;

use serde::{Deserialize, Serialize};

use crate::bus::{PANIC_TEXT_MAX, PanicStr, UpType};
use crate::crc::crc32;
use crate::host_owner::{Holder, Input};
use crate::link::{
    BridgeToHost, CobsStream, LinkError, LoopPhase, MAX_FRAME, ShortStr, decode_frame, encode_frame,
};
use crate::ota::image::DIGEST_LEN;
use crate::reset::ResetCause;
use crate::wardrive_esp::line::LINE_MAX;
use crate::wardrive_esp::session::Class;

/// The layout of the log's records, which [`Opened`] names first in every file.
pub const FORMAT_VERSION: u8 = 1;

/// A record's kind and uptime, ahead of its payload.
const HEADER: usize = 5;

/// A record's CRC, behind its payload.
const CRC: usize = 4;

/// The longest payload: a link frame of [`MAX_FRAME`] without its terminator.
pub const PAYLOAD_MAX: usize = MAX_FRAME - 1;

/// The longest record body, ahead of COBS.
const BODY_MAX: usize = HEADER + PAYLOAD_MAX + CRC;

/// The longest record, terminator included. Any [`BridgeToHost`] frame, a fullest `Status`
/// included, fits one.
pub const RECORD_MAX: usize = cobs::max_encoding_length(BODY_MAX) + 1;

/// The longest [`Opened`] payload: every varint and string at its longest.
// The version, then `file`, `part` and `opened_ms` as varints, `fw_version` behind its length,
// `reset_cause` and `last_phase`, `slot` behind its length, `trial`, `image` behind its tag,
// `card_mb`, `free_mb` behind its tag, and `last_panic` behind its tag and a two-byte length.
const OPENED_MAX: usize = 1
    + 5
    + 3
    + 10
    + (1 + 32)
    + 1
    + 1
    + (1 + 32)
    + 1
    + (1 + DIGEST_LEN)
    + 5
    + 6
    + (1 + 2 + PANIC_TEXT_MAX);

const _: () = assert!(OPENED_MAX <= PAYLOAD_MAX, "an Opened record must fit");
// The class byte, then the line.
const _: () = assert!(LINE_MAX < PAYLOAD_MAX, "a phone's longest line must fit a record");

/// What a record is, its first byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// The file's header, [`Opened`].
    Opened = 1,
    /// A frame the bridge produced for its host.
    Frame = 2,
    /// A `Status` the log built itself.
    Snapshot = 3,
    /// A line the bridge sent a phone.
    PhoneLine = 4,
    /// A command a phone wrote.
    PhoneCommand = 5,
    /// A change of who holds the host queue.
    Owner = 6,
    /// Records the log lost.
    Dropped = 7,
    /// The file's end.
    Closed = 8,
}

impl Kind {
    /// The kind's byte.
    #[must_use]
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// The kind whose byte is `code`, or `None` for one this build does not know.
    #[must_use]
    pub const fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::Opened,
            2 => Self::Frame,
            3 => Self::Snapshot,
            4 => Self::PhoneLine,
            5 => Self::PhoneCommand,
            6 => Self::Owner,
            7 => Self::Dropped,
            8 => Self::Closed,
            _ => return None,
        })
    }
}

/// The first record of every file: which boot wrote it, and onto what card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Opened {
    /// The records' layout, [`FORMAT_VERSION`].
    pub version: u8,
    /// The file's number, its name's digits.
    pub file: u32,
    /// Which of this boot's files it is: 0 for the first, one more at each rotation.
    pub part: u16,
    /// The P4's uptime when it opened the file, in full.
    pub opened_ms: u64,
    /// The bridge's firmware version.
    pub fw_version: ShortStr,
    /// Why this boot started.
    pub reset_cause: ResetCause,
    /// Where the previous boot stopped, when the reset preserved it.
    pub last_phase: LoopPhase,
    /// The app slot the image booted from.
    pub slot: ShortStr,
    /// Whether the image is on trial.
    pub trial: bool,
    /// The booted image's digest, when the P4 read one.
    pub image: Option<[u8; DIGEST_LEN]>,
    /// The card's size in MiB.
    pub card_mb: u32,
    /// The volume's free space in MiB when the file opened, when the P4 knew it.
    pub free_mb: Option<u32>,
    /// The text of the panic that ended the previous boot, when the reset preserved it.
    pub last_panic: Option<PanicStr>,
}

/// Why a file ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CloseReason {
    /// Logging was turned off.
    SettingOff = 0,
    /// The file reached its size, and the next one takes over.
    Rotation = 1,
}

impl CloseReason {
    const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::SettingOff),
            1 => Some(Self::Rotation),
            _ => None,
        }
    }
}

/// A phone line's class byte.
const fn class_code(class: Class) -> u8 {
    match class {
        Class::Answer => 0,
        Class::Stream => 1,
    }
}

const fn class_from_code(code: u8) -> Option<Class> {
    match code {
        0 => Some(Class::Answer),
        1 => Some(Class::Stream),
        _ => None,
    }
}

/// Whether the log keeps `msg`. Sighting batches, update statuses, GPS bytes and delivery
/// outcomes are the bulk of the host stream and a capture's to keep, so the log leaves them out.
/// It keeps a node's heartbeats and panic records: they say how each node fared, at a few
/// records a second for the whole fleet.
#[must_use]
pub const fn logged(msg: &BridgeToHost) -> bool {
    // No `_` arm: a new message is a decision, not a default.
    match msg {
        BridgeToHost::FromNode { kind, .. } => {
            *kind == UpType::Heartbeat.as_u8() || *kind == UpType::Panic.as_u8()
        }
        BridgeToHost::Gps { .. }
        | BridgeToHost::Delivered { .. }
        | BridgeToHost::Undelivered { .. } => false,
        BridgeToHost::Ready { .. }
        | BridgeToHost::Status { .. }
        | BridgeToHost::Log { .. }
        | BridgeToHost::Error { .. }
        | BridgeToHost::Update(_)
        | BridgeToHost::PowerCycled { .. }
        | BridgeToHost::CacheImage(_)
        | BridgeToHost::Fleet(_)
        | BridgeToHost::SdFormatted { .. } => true,
    }
}

/// Write a record of `kind` at uptime `ms` into `out`, its payload `parts` one after another,
/// terminator included. Returns how many bytes of `out` it used, or `None` when the payload is
/// longer than [`PAYLOAD_MAX`] or `out` cannot hold the record.
fn encode_parts(kind: Kind, ms: u32, parts: &[&[u8]], out: &mut [u8]) -> Option<usize> {
    let mut body = [0u8; BODY_MAX];
    body[0] = kind.code();
    body[1..HEADER].copy_from_slice(&ms.to_le_bytes());
    let mut end = HEADER;
    for part in parts {
        let to = end.checked_add(part.len()).filter(|&to| to <= HEADER + PAYLOAD_MAX)?;
        body[end..to].copy_from_slice(part);
        end = to;
    }
    let crc = crc32(&body[..end]);
    body[end..end + CRC].copy_from_slice(&crc.to_le_bytes());
    let body = &body[..end + CRC];
    if out.is_empty() {
        return None;
    }
    let mut stream = CobsStream::new();
    let written = stream.fill(body, out);
    stream.is_done().then_some(written)
}

/// Write a record of `kind` at uptime `ms` with `payload` into `out`, terminator included.
///
/// A `Frame` or `Snapshot` payload is a link frame as [`encode_frame`] wrote it, without its
/// terminator; [`encode_message`] encodes one from the message.
///
/// Returns how many bytes of `out` it used, or `None` when `payload` is longer than
/// [`PAYLOAD_MAX`] or `out` cannot hold the record.
pub fn encode(kind: Kind, ms: u32, payload: &[u8], out: &mut [u8]) -> Option<usize> {
    encode_parts(kind, ms, &[payload], out)
}

/// Write a `Frame` or `Snapshot` record of `msg`, encoding its link frame first. Returns what
/// [`encode`] does, and `None` when `msg` does not encode.
pub fn encode_message(kind: Kind, msg: &BridgeToHost, ms: u32, out: &mut [u8]) -> Option<usize> {
    let mut frame = [0u8; MAX_FRAME];
    let len = encode_frame(msg, &mut frame).ok()?;
    // The frame ends in its terminator, which the record leaves out.
    encode(kind, ms, &frame[..len - 1], out)
}

/// Write the [`Opened`] record. Returns what [`encode`] does.
pub fn encode_opened(opened: &Opened, ms: u32, out: &mut [u8]) -> Option<usize> {
    let mut payload = [0u8; OPENED_MAX];
    let used = postcard::to_slice(opened, &mut payload).ok()?.len();
    encode(Kind::Opened, ms, &payload[..used], out)
}

/// Write a `PhoneLine` record. Returns what [`encode`] does.
pub fn encode_phone_line(class: Class, line: &[u8], ms: u32, out: &mut [u8]) -> Option<usize> {
    encode_parts(Kind::PhoneLine, ms, &[&[class_code(class)], line], out)
}

/// Write an `Owner` record: `input` moved the queue from `before` to `after`. Returns what
/// [`encode`] does.
pub fn encode_owner(
    input: Input,
    before: Holder,
    after: Holder,
    ms: u32,
    out: &mut [u8],
) -> Option<usize> {
    // Three variant indices, one byte each.
    let mut payload = [0u8; 3];
    let used = postcard::to_slice(&(input, before, after), &mut payload).ok()?.len();
    encode(Kind::Owner, ms, &payload[..used], out)
}

/// Write a `Dropped` record of `count` lost records. Returns what [`encode`] does.
pub fn encode_dropped(count: u32, ms: u32, out: &mut [u8]) -> Option<usize> {
    encode(Kind::Dropped, ms, &count.to_le_bytes(), out)
}

/// Write a `Closed` record. Returns what [`encode`] does.
pub fn encode_closed(reason: CloseReason, ms: u32, out: &mut [u8]) -> Option<usize> {
    encode(Kind::Closed, ms, &[reason as u8], out)
}

/// The records of `file`, as ranges without their terminators. Runs of zeros, from padding or a
/// cut-off write, yield nothing, and bytes after the last terminator yield one last range, which
/// [`decode`] reports.
pub fn records(file: &[u8]) -> impl Iterator<Item = Range<usize>> + '_ {
    let mut at = 0;
    core::iter::from_fn(move || {
        while at < file.len() {
            let start = at;
            let end = file[start..].iter().position(|&b| b == 0).map_or(file.len(), |k| start + k);
            at = end + 1;
            if end > start {
                return Some(start..end);
            }
        }
        None
    })
}

/// One decoded record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry<'a> {
    /// Its kind's byte.
    pub kind: u8,
    /// The P4's uptime when it was recorded, its low 32 bits; [`Clock`] unwraps it.
    pub ms: u32,
    /// What it holds.
    pub body: Body<'a>,
}

/// What a record holds.
// Unboxed: the crate allocates nothing, and a reader holds one at a time.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body<'a> {
    /// The file's header.
    Opened(Opened),
    /// A frame the bridge produced for its host.
    Frame(BridgeToHost),
    /// A `Status` the log built itself.
    Snapshot(BridgeToHost),
    /// A line the bridge sent a phone.
    PhoneLine {
        /// How the P4 queued it.
        class: Class,
        /// The line's bytes.
        line: &'a [u8],
    },
    /// The bytes a phone wrote.
    PhoneCommand(&'a [u8]),
    /// A change of who holds the host queue.
    Owner {
        /// What moved it.
        input: Input,
        /// Who held it.
        before: Holder,
        /// Who holds it now.
        after: Holder,
    },
    /// Records the log lost since the last `Dropped`.
    Dropped(u32),
    /// The file's end.
    Closed(CloseReason),
    /// A kind this build does not know, with its payload.
    Unknown {
        /// Its kind's byte.
        kind: u8,
        /// Its payload.
        bytes: &'a [u8],
    },
}

/// Why a record did not decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bad {
    /// Its COBS did not decode.
    Cobs,
    /// It is shorter than a kind, an uptime and a CRC.
    Short,
    /// Its CRC does not match: a torn write, or bytes from before the card was reused.
    Crc,
    /// Its link frame did not decode.
    Frame(LinkError),
    /// Its CRC matched, but its payload is not its kind's.
    Malformed,
}

/// Decode `record`, a range [`records`] gave, in place.
///
/// # Errors
/// See [`Bad`].
pub fn decode(record: &mut [u8]) -> Result<Entry<'_>, Bad> {
    let len = cobs::decode_in_place(record).map_err(|_| Bad::Cobs)?;
    if len < HEADER + CRC {
        return Err(Bad::Short);
    }
    let (data, crc) = record[..len].split_at_mut(len - CRC);
    if crc32(data) != u32::from_le_bytes([crc[0], crc[1], crc[2], crc[3]]) {
        return Err(Bad::Crc);
    }
    let (head, payload) = data.split_at_mut(HEADER);
    let kind = head[0];
    let ms = u32::from_le_bytes([head[1], head[2], head[3], head[4]]);
    let body = match Kind::from_code(kind) {
        Some(Kind::Opened) => {
            if payload.first() != Some(&FORMAT_VERSION) {
                return Err(Bad::Malformed);
            }
            Body::Opened(exact(payload)?)
        }
        Some(Kind::Frame) => Body::Frame(decode_frame(payload).map_err(Bad::Frame)?),
        Some(Kind::Snapshot) => Body::Snapshot(decode_frame(payload).map_err(Bad::Frame)?),
        Some(Kind::PhoneLine) => {
            let (&class, line) = payload.split_first().ok_or(Bad::Malformed)?;
            Body::PhoneLine { class: class_from_code(class).ok_or(Bad::Malformed)?, line }
        }
        Some(Kind::PhoneCommand) => Body::PhoneCommand(payload),
        Some(Kind::Owner) => {
            let (input, before, after) = exact(payload)?;
            Body::Owner { input, before, after }
        }
        Some(Kind::Dropped) => {
            let count: [u8; 4] = (*payload).try_into().map_err(|_| Bad::Malformed)?;
            Body::Dropped(u32::from_le_bytes(count))
        }
        Some(Kind::Closed) => match *payload {
            [code] => Body::Closed(CloseReason::from_code(code).ok_or(Bad::Malformed)?),
            _ => return Err(Bad::Malformed),
        },
        None => Body::Unknown { kind, bytes: payload },
    };
    Ok(Entry { kind, ms, body })
}

/// `payload` as one postcard `T`, with nothing after it.
fn exact<'a, T: Deserialize<'a>>(payload: &'a [u8]) -> Result<T, Bad> {
    match postcard::take_from_bytes(payload) {
        Ok((value, [])) => Ok(value),
        _ => Err(Bad::Malformed),
    }
}

/// The P4's full uptime from the 32-bit one each record carries.
///
/// Each value is read as the one nearest the last, within 2³¹ ms either way. A file opened with no
/// card in the slot starts with the records the bridge held meanwhile, which are older than its
/// [`Opened`], so a value below the last is not always a wrap: it is one when it lies nearer the
/// next 2³² ms than the last value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    last: u64,
}

impl Clock {
    /// A clock at [`Opened::opened_ms`].
    #[must_use]
    pub const fn new(opened_ms: u64) -> Self {
        Self { last: opened_ms }
    }

    /// The full uptime of a record that carries `ms`.
    pub fn at(&mut self, ms: u32) -> u64 {
        // The low 32 bits of the last value, and the signed step to `ms`.
        let step = ms.wrapping_sub(self.last as u32) as i32;
        let size = u64::from(step.unsigned_abs());
        self.last = if step >= 0 {
            self.last.saturating_add(size)
        } else {
            // A step back past 0 has no earlier lap to land in: `ms` is the first lap's.
            self.last.checked_sub(size).unwrap_or(u64::from(ms))
        };
        self.last
    }
}
