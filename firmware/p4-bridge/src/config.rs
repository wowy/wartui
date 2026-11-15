//! The P4 bridge's queue sizes, in one place.
//!
//! Each size is read from a `WARTUI_P4_*` environment variable at build time, with the
//! default here, so a bench run resizes one with a rebuild rather than an edit.
//! `wartui_proto::config` has the parsing.
//!
//! esp-hal's default 256 KB L2 cache leaves the P4 about 439 KB of internal RAM, and the main
//! stack gets what the statics leave. The host bulk ring is the one queue in PSRAM, past the node
//! image cache (`src/cache.rs`), for three reasons (`docs/c6-ble-host-findings.md`, run 15):
//!
//! - it frees 240 KB of internal RAM;
//! - at 1 MB it holds two 500-batch bursts, which a Bluetooth host drains far slower than UART0;
//! - it bounds the lag. A sighting is positioned when it reaches the host, so the ring's size is
//!   the worst lag: ~27 s at the slowest central measured, 39.1 KB/s.
//!
//! PSRAM costs UART0 nothing measurable: it carried 391 KB/s of 4 Mbaud's 400 KB/s from it. A bench
//! `wartui status` reports each queue's high-water mark, which is how to right-size them.

use wartui_proto::config::env_usize;

/// Bytes of encoded frames bound for the host that may be evicted when it falls behind:
/// node records, GPS bytes and logs. Taken from the PSRAM the node image cache leaves, and fewer
/// when less is mapped.
pub const HOST_BULK_BYTES: usize = env_usize(option_env!("WARTUI_P4_HOST_BULK_BYTES"), 1024 * 1024);

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
