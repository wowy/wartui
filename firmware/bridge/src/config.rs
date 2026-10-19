//! The bridge's queue sizes, in one place.
//!
//! Each is read from a `WARTUI_BRIDGE_*` environment variable at build time, with the
//! default here, so a bench run can resize one with a rebuild rather than an edit.
//! `wartui_proto::config` has the parsing.

use wartui_proto::config::env_usize;

/// The sends whose payloads can wait for the radio: the pool `wartui status` reports as
/// `radio-send-jobs`. Each slot holds a full ESP-NOW v2 payload.
///
/// A send that finds the pool full is answered `Rejected` and counted in `Status.queues`,
/// whose high-water mark is what to size this from.
pub const SEND_JOBS: usize = env_usize(option_env!("WARTUI_BRIDGE_SEND_JOBS"), 4);

/// The radio jobs — sends, and peer and power commands — that can wait for the radio in
/// arrival order, and the results that can wait for the loop: `radio-jobs` in
/// `wartui status`.
///
/// Each entry is a few bytes, so this holds a whole fleet's worth: the host removes every
/// departed node's peer in one batch, and the twenty-node peer table bounds that batch. A
/// job that finds the queue full is refused at once (a send `Rejected`, any other with an
/// `Error` line) and counted in `Status.queues`.
pub const RADIO_JOBS: usize = env_usize(option_env!("WARTUI_BRIDGE_RADIO_JOBS"), 32);
