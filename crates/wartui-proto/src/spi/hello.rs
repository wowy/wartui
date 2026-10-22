//! The frozen hello each side of the SPI link sends first.
//!
//! **Its layout never changes**, so any P4 build reads any C6 build's hello. That is why it
//! is a fixed layout written out by hand rather than postcard, whose encoding follows the
//! types. Little-endian:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 2 | `link_rev` |
//! | 2 | 1 | `chip` |
//! | 3 | 6 | `mac` |
//! | 9 | 32 | `fw_version`, UTF-8, zero-padded |
//! | 41 | 1 | `reset_cause` |
//! | 42 | 1 | `last_phase` |
//! | 43 | 4 | `uptime_ms` |
//! | 47 | 4 | `heap_free` |
//! | 51 | 32 | `image`, when present: the SHA-256 digest of the running app image |
//!
//! The enums travel as their declaration-order index, the index postcard writes. Their
//! codes are spelled out in `match` arms below rather than cast, so the hello's codes stay
//! put whatever happens to the enums. The link enums only grow at the end, so a code never
//! changes meaning. A cause or phase this build cannot name reads as `Unknown`, since they
//! are diagnostics. A chip it cannot name fails the decode: the hello is then from no peer
//! this build can talk to.
//!
//! Fields are appended, never inserted, so the frozen part never moves. Decoders read the
//! fields they know and ignore the rest. A C6 that takes updates over the link appends
//! `image`; a 51-byte hello comes from one that does not. A C6 that takes updates but cannot
//! read its own digest sends zeros, which name no image, so the P4 still updates it.

use crate::link::{Chip, LoopPhase, ShortStr};
use crate::mac::Mac;
use crate::ota::image::DIGEST_LEN;
use crate::reset::ResetCause;

/// Bytes in an encoded [`Hello`] without [`Hello::image`].
pub const HELLO_LEN: usize = 51;

/// Bytes in an encoded [`Hello`] with [`Hello::image`].
pub const HELLO_IMAGE_LEN: usize = HELLO_LEN + DIGEST_LEN;

const FW_VERSION: usize = 9;
const FW_VERSION_LEN: usize = 32;

/// Who is on the other end of the link, sent in a [`Kind::Hello`](super::Kind::Hello)
/// transfer.
///
/// The P4 sends one too, with [`Chip::Esp32P4`]. The C6 reads only its `link_rev`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    /// The sender's [`LINK_REVISION`](super::LINK_REVISION).
    pub link_rev: u16,
    /// The sender's chip.
    pub chip: Chip,
    /// The sender's MAC.
    pub mac: Mac,
    /// The sender's firmware version.
    pub fw_version: ShortStr,
    /// Why the sender's current life started.
    pub reset_cause: ResetCause,
    /// Where the sender's previous life stopped, when the reset preserved it.
    pub last_phase: LoopPhase,
    /// Milliseconds since the sender's life started.
    pub uptime_ms: u32,
    /// Bytes free in the sender's heap.
    pub heap_free: u32,
    /// The digest espflash appends to the running app image
    /// ([`crate::ota::image::appended_digest`]), which names the build. `None` from a sender
    /// that takes no update over this link, the P4 among them: its updates come over USB.
    pub image: Option<[u8; DIGEST_LEN]>,
}

impl Hello {
    /// Writes the hello to the start of `out` and returns its length: [`HELLO_IMAGE_LEN`]
    /// with an image, [`HELLO_LEN`] without.
    ///
    /// # Panics
    /// If `out` is shorter than that.
    pub fn encode(&self, out: &mut [u8]) -> usize {
        if let Some(image) = &self.image {
            out[HELLO_LEN..HELLO_IMAGE_LEN].copy_from_slice(image);
        }
        let out = &mut out[..HELLO_LEN];
        out[0..2].copy_from_slice(&self.link_rev.to_le_bytes());
        out[2] = chip_code(self.chip);
        out[3..9].copy_from_slice(&self.mac);
        let version = &mut out[FW_VERSION..FW_VERSION + FW_VERSION_LEN];
        version.fill(0);
        version[..self.fw_version.len()].copy_from_slice(self.fw_version.as_bytes());
        out[41] = reset_cause_code(self.reset_cause);
        out[42] = loop_phase_code(self.last_phase);
        out[43..47].copy_from_slice(&self.uptime_ms.to_le_bytes());
        out[47..51].copy_from_slice(&self.heap_free.to_le_bytes());
        if self.image.is_some() { HELLO_IMAGE_LEN } else { HELLO_LEN }
    }

    /// Reads a hello from the start of `payload`.
    ///
    /// `None` when `payload` is shorter than [`HELLO_LEN`] or the chip is one this build
    /// cannot name. A version that is not UTF-8 is cut at its last valid character. The image
    /// is read when `payload` holds [`HELLO_IMAGE_LEN`] bytes or more.
    #[must_use]
    pub fn decode(payload: &[u8]) -> Option<Self> {
        let b = payload.get(..HELLO_LEN)?;
        let version = &b[FW_VERSION..FW_VERSION + FW_VERSION_LEN];
        let version_len = version.iter().position(|&c| c == 0).unwrap_or(FW_VERSION_LEN);
        let version = &version[..version_len];
        // A version that is not UTF-8 is cut at its last valid character rather than
        // failing the decode, so a P4 can always identify the C6.
        let valid = match core::str::from_utf8(version) {
            Ok(text) => text,
            Err(error) => core::str::from_utf8(&version[..error.valid_up_to()]).ok()?,
        };
        let fw_version = valid.try_into().ok()?;
        Some(Self {
            link_rev: u16::from_le_bytes([b[0], b[1]]),
            chip: chip_from_code(b[2])?,
            mac: [b[3], b[4], b[5], b[6], b[7], b[8]],
            fw_version,
            reset_cause: reset_cause_from_code(b[41]),
            last_phase: loop_phase_from_code(b[42]),
            uptime_ms: u32::from_le_bytes([b[43], b[44], b[45], b[46]]),
            heap_free: u32::from_le_bytes([b[47], b[48], b[49], b[50]]),
            image: payload[HELLO_LEN..].first_chunk().copied(),
        })
    }
}

const fn chip_code(chip: Chip) -> u8 {
    match chip {
        Chip::Esp32C5 => 0,
        Chip::Esp32C6 => 1,
        Chip::Esp32P4 => 2,
    }
}

const fn chip_from_code(code: u8) -> Option<Chip> {
    match code {
        0 => Some(Chip::Esp32C5),
        1 => Some(Chip::Esp32C6),
        2 => Some(Chip::Esp32P4),
        _ => None,
    }
}

const fn reset_cause_code(cause: ResetCause) -> u8 {
    match cause {
        ResetCause::PowerOn => 0,
        ResetCause::Software => 1,
        ResetCause::Watchdog => 2,
        ResetCause::Lockup => 3,
        ResetCause::Brownout => 4,
        ResetCause::External => 5,
        ResetCause::Unknown => 6,
    }
}

const fn reset_cause_from_code(code: u8) -> ResetCause {
    match code {
        0 => ResetCause::PowerOn,
        1 => ResetCause::Software,
        2 => ResetCause::Watchdog,
        3 => ResetCause::Lockup,
        4 => ResetCause::Brownout,
        5 => ResetCause::External,
        _ => ResetCause::Unknown,
    }
}

const fn loop_phase_code(phase: LoopPhase) -> u8 {
    match phase {
        LoopPhase::Unknown => 0,
        LoopPhase::Boot => 1,
        LoopPhase::DrainRadio => 2,
        LoopPhase::DrainLink => 3,
        LoopPhase::Command => 4,
        LoopPhase::Transmit => 5,
        LoopPhase::Pump => 6,
        LoopPhase::Idle => 7,
        LoopPhase::TxStalled => 8,
        LoopPhase::Render => 9,
        LoopPhase::SpiTransfer => 10,
    }
}

const fn loop_phase_from_code(code: u8) -> LoopPhase {
    match code {
        1 => LoopPhase::Boot,
        2 => LoopPhase::DrainRadio,
        3 => LoopPhase::DrainLink,
        4 => LoopPhase::Command,
        5 => LoopPhase::Transmit,
        6 => LoopPhase::Pump,
        7 => LoopPhase::Idle,
        8 => LoopPhase::TxStalled,
        9 => LoopPhase::Render,
        10 => LoopPhase::SpiTransfer,
        _ => LoopPhase::Unknown,
    }
}
