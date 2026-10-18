//! The bridge's queue sizes, in one place.
//!
//! Each is read from a `WARTUI_BRIDGE_*` environment variable at build time, with the
//! default here, so a bench run can resize one with a rebuild rather than an edit.
//! `wartui_proto::config` has the parsing.

use wartui_proto::config::env_usize;

/// The radio jobs — sends, and peer and power commands — that can wait for the radio, and
/// the results that can wait for the loop. Named for the queue `wartui status` reports.
///
/// A job that finds the queue full is refused at once (a send `Rejected`, any other with an
/// `Error` line) and counted in `Status.queues`, whose high-water mark is what to size this
/// from. A peer or power command waits for the jobs ahead of it and then takes microseconds,
/// so the host's `SetTxPower` on every status poll holds a slot only briefly.
pub const SEND_JOBS: usize = env_usize(option_env!("WARTUI_BRIDGE_SEND_JOBS"), 4);
