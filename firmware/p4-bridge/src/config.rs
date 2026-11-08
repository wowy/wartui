//! The P4 bridge's queue sizes, in one place.
//!
//! Each size is read from a `WARTUI_P4_*` environment variable at build time, with the
//! default here, so a bench run resizes one with a rebuild rather than an edit.
//! `wartui_proto::config` has the parsing.
//!
//! esp-hal's default 256 KB L2 cache leaves the P4 about 439 KB of internal RAM, and the
//! spike ran a 300 KB host queue in it, so these defaults fit without PSRAM. The main stack gets
//! what the statics leave, about 32 KB with these defaults. A bench `wartui status` reports each
//! queue's high-water mark, which is how to right-size them: the host bulk queue peaks well under
//! 1% of its size on a carrier.

use wartui_proto::config::env_usize;

/// Bytes of encoded frames bound for the host that may be evicted when it falls behind:
/// node records, GPS bytes and logs.
pub const HOST_BULK_BYTES: usize = env_usize(option_env!("WARTUI_P4_HOST_BULK_BYTES"), 240 * 1024);

/// Bytes of encoded frames bound for the host that answer it: `Ready`, `Status`, delivery
/// answers and errors.
pub const HOST_PRIORITY_BYTES: usize =
    env_usize(option_env!("WARTUI_P4_HOST_PRIORITY_BYTES"), 16 * 1024);

/// Bytes of encoded `Down` records waiting for a packet to the C6: the status poll, a byte or
/// two each.
pub const DOWN_QUEUE_BYTES: usize = env_usize(option_env!("WARTUI_P4_DOWN_QUEUE_BYTES"), 1024);

/// Bytes of records the host sent one node that wait behind the flights out in its slot's queue.
/// The default holds four full update chunks, 1,034 bytes each with their framing: the sector an
/// update session sends at once, before the bus has taken any.
///
/// A record that finds the queue full is refused at once, with `Undelivered`.
pub const NODE_DOWN_BYTES: usize = env_usize(option_env!("WARTUI_P4_NODE_DOWN_BYTES"), 4160);
