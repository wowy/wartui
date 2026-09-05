//! Wire formats for the ESP32-C5 wardriver mesh.
//!
//! This crate is `no_std` and allocation-free because it is compiled into both
//! the host TUI and the bridge firmware. Defining the wire types once is the
//! only thing that keeps the two ends from drifting apart.
//!
//! Everything here mirrors the C++ firmware at `ESP32DualBandWardriver`
//! (branch `feat/node-interference-mitigation`). Source references in the docs
//! are `file:line` into that repo.

#![no_std]

pub mod air;
pub mod link;
pub mod plan;

/// Re-exported so consumers can build link payloads without depending on
/// `heapless` themselves, and can never end up on a mismatched version.
pub use heapless;

pub use air::{AdminMsg, DecodeError, Frame, MsgType, TextMsg, WardriveLine};
pub use link::{BridgeToHost, HostToBridge, LinkError, SendStatus};
pub use plan::{ChannelPool, SCAN_CHANNELS};
