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

/// The send jobs that can wait for the radio, and the results that can wait for the link.
///
/// A job that finds the queue full is answered `Rejected` and counted in
/// `RadioStatus.queues`.
pub const SEND_JOBS: usize = env_usize(option_env!("WARTUI_RADIO_SEND_JOBS"), 4);
