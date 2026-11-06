//! The C6 firmware's queue size, in one place.
//!
//! It is read from a `WARTUI_RADIO_*` environment variable at build time, with the default
//! here, so a bench run resizes it with a rebuild rather than an edit.
//! `wartui_proto::config` has the parsing.
//!
//! A bench `wartui status` reports the queue's high-water mark, which is how to right-size it.

use wartui_proto::config::env_usize;

/// Bytes of encoded `Up` records — the C6's lines and its status — waiting for a packet to
/// the P4.
///
/// A full queue evicts its oldest record, counted in `RadioStatus.queues`. These records are
/// small and few, so a backlog this deep means the link has stopped.
pub const UP_QUEUE_BYTES: usize = env_usize(option_env!("WARTUI_RADIO_UP_QUEUE_BYTES"), 8 * 1024);
