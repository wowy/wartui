//! The P4 bridge's queue sizes, the microSD card's clock and the SD log's sizes, in one place.
//!
//! Each is read from a `WARTUI_P4_*` environment variable at build time, with the default
//! here, so a bench run changes one with a rebuild rather than an edit.
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

/// The microSD card's bus clock asked for, in Hz (`src/sd.rs`). The driver caps it at 40 MHz, and
/// `sdio` switches the card to high speed above 25 MHz. At 40 MHz the bench card failed about one
/// write in 6,100 with a CRC error, and at 20 MHz none in 24,000 (`docs/p4-sd-findings.md`).
pub const SD_HZ: u32 = env_usize(option_env!("WARTUI_P4_SD_HZ"), 20_000_000) as u32;

/// Bytes of encoded records the SD log holds in PSRAM until the card takes them, the newest kept
/// while no card does (`src/sd_log.rs`). Split off the PSRAM after the host bulk ring, and fewer
/// when less is left.
pub const SD_LOG_BYTES: usize = env_usize(option_env!("WARTUI_P4_SD_LOG_BYTES"), 256 * 1024);

/// The size at which the SD log closes its file and opens the next, in bytes.
pub const SD_LOG_FILE_MAX: u32 =
    env_usize(option_env!("WARTUI_P4_SD_LOG_FILE_MAX"), 64 * 1024 * 1024) as u32;

/// The free space the SD log leaves on the card, in MiB, deleting its oldest files to keep it.
/// Capped at an eighth of the volume (`wartui_proto::sd::append::Policy`).
pub const SD_LOG_KEEP_FREE_MB: u32 =
    env_usize(option_env!("WARTUI_P4_SD_LOG_KEEP_FREE_MB"), 256) as u32;
