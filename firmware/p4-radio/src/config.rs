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

/// Bytes of encoded `Up` records waiting for a transfer to the P4.
///
/// A full queue evicts its oldest record, counted in `RadioStatus.queues`.
pub const UP_QUEUE_BYTES: usize = env_usize(option_env!("WARTUI_RADIO_UP_QUEUE_BYTES"), 96 * 1024);

/// The radio jobs — sends, and peer and power commands — that can wait for the radio, and
/// the results that can wait for the link. Named for the queue `wartui status` reports.
///
/// A job that finds the queue full is refused at once (a send `Rejected`, any other with an
/// `Error` line) and counted in `RadioStatus.queues`. A peer or power command waits for the
/// jobs ahead of it and then takes microseconds, so it holds a slot only briefly.
pub const SEND_JOBS: usize = env_usize(option_env!("WARTUI_RADIO_SEND_JOBS"), 4);
