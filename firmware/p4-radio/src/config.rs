//! The radio firmware's queue sizes, in one place.
//!
//! Each is read from a `WARTUI_RADIO_*` environment variable at build time, with the
//! default here, so a bench run resizes one with a rebuild rather than an edit.
//! `wartui_proto::config` has the parsing.
//!
//! The C6 has 512 KB of SRAM and the radio blobs' heap takes 100 KB, so these defaults
//! leave room. A bench `wartui status` reports each queue's high-water mark, which is
//! how to right-size them.

use wartui_proto::config::env_usize;

/// Bytes of received frames, as encoded `Up` records, waiting for a transfer to the P4.
///
/// A full queue evicts its oldest record, counted in `RadioStatus.queues`.
pub const UP_QUEUE_BYTES: usize = env_usize(option_env!("WARTUI_RADIO_UP_QUEUE_BYTES"), 96 * 1024);

/// Bytes of every other encoded `Up` record — send results, lines and the C6's status —
/// waiting for a transfer to the P4, ahead of the received frames.
///
/// A full queue evicts its oldest record, counted in `RadioStatus.queues`. These records
/// are small and few, so a backlog this deep means the link has stopped.
pub const UP_PRIORITY_BYTES: usize =
    env_usize(option_env!("WARTUI_RADIO_UP_PRIORITY_BYTES"), 8 * 1024);

/// The sends whose payloads can wait for the radio: the pool `wartui status` reports as
/// `radio-send-jobs`. Each slot holds a full ESP-NOW v2 payload.
///
/// A send that finds the pool full is answered `Rejected` and counted in
/// `RadioStatus.queues`.
pub const SEND_JOBS: usize = env_usize(option_env!("WARTUI_RADIO_SEND_JOBS"), 4);

/// The radio jobs — sends, and peer and power commands — that can wait for the radio in
/// arrival order, and the results that can wait for the link: `radio-jobs` in
/// `wartui status`.
///
/// Each entry is a few bytes, so this holds a whole fleet's worth: the host removes every
/// departed node's peer in one batch, which one transfer can carry whole. A job that finds
/// the queue full is refused at once (a send `Rejected`, any other with an `Error` line) and
/// counted in `RadioStatus.queues`.
pub const RADIO_JOBS: usize = env_usize(option_env!("WARTUI_RADIO_JOBS"), 32);
