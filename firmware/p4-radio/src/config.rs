//! The C6 firmware's queue sizes, in one place.
//!
//! Each is read from a `WARTUI_RADIO_*` environment variable at build time, with the default
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

/// Bytes of the host stream held for a Bluetooth central until they are notified.
///
/// The P4 never has more than its own `BLE_WINDOW_BYTES` uncredited
/// (`firmware/p4-bridge/src/config.rs`), so this matches its default. A smaller one drops what
/// overflows, counted in `BleStats.dropped_overflow`.
pub const BLE_WINDOW_BYTES: usize =
    env_usize(option_env!("WARTUI_RADIO_BLE_WINDOW_BYTES"), 16 * 1024);
