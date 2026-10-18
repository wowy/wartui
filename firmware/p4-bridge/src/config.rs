//! The P4 bridge's queue sizes and link clock, in one place.
//!
//! Each size is read from a `WARTUI_P4_*` environment variable at build time, with the
//! default here, so a bench run resizes one with a rebuild rather than an edit.
//! `wartui_proto::config` has the parsing.
//!
//! esp-hal's default 256 KB L2 cache leaves the P4 about 439 KB of internal RAM, and the
//! spike ran a 300 KB host queue in it, so these defaults fit without PSRAM. A bench
//! `wartui status` reports each queue's high-water mark, which is how to right-size them.

use wartui_proto::config::env_usize;

/// Bytes of encoded frames bound for the host that may be evicted when it falls behind:
/// received frames and logs.
pub const HOST_BULK_BYTES: usize = env_usize(option_env!("WARTUI_P4_HOST_BULK_BYTES"), 256 * 1024);

/// Bytes of encoded frames bound for the host that answer it: `Ready`, `Status`, send
/// results and errors.
pub const HOST_PRIORITY_BYTES: usize =
    env_usize(option_env!("WARTUI_P4_HOST_PRIORITY_BYTES"), 16 * 1024);

/// Bytes of encoded `Down` records waiting for a transfer to the C6.
pub const DOWN_QUEUE_BYTES: usize = env_usize(option_env!("WARTUI_P4_DOWN_QUEUE_BYTES"), 16 * 1024);

/// The SPI clock. Not configurable: it is the rate the spike measured clean, and SPI2's
/// 160 MHz function clock divides to it exactly.
pub const SPI_KHZ: u32 = 40_000;
