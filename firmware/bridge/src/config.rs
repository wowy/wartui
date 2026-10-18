//! The bridge's queue sizes, in one place.
//!
//! Each is read from a `WARTUI_BRIDGE_*` environment variable at build time, with the
//! default here, so a bench run can resize one with a rebuild rather than an edit.
//! `wartui_proto::config` has the parsing.

use wartui_proto::config::env_usize;

/// The send jobs that can wait for the radio, and the results that can wait for the loop.
///
/// A job that finds the queue full is answered `Rejected` and counted in `Status.queues`,
/// whose high-water mark is what to size this from.
pub const SEND_JOBS: usize = env_usize(option_env!("WARTUI_BRIDGE_SEND_JOBS"), 4);
