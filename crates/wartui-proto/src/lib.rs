//! Wire formats for the ESP32-C5 wardriver mesh.
//!
//! This crate is `no_std` and allocation-free because it is compiled into both
//! the host TUI and the bridge firmware. Defining the wire types once is the
//! only thing that keeps the two ends from drifting apart.
//!
//! The wire formats are wartui's own. Where a doc comment here cites
//! `src/*.cpp:NNN` it is pointing into the vendor firmware this project grew up
//! against — <https://github.com/wowy/ESP32DualBandWardriver>, branch
//! `feat/node-interference-mitigation` — as the record of a *measured
//! behaviour* that a design decision here answers. Nothing in this crate
//! interoperates with it, and from the frame layouts up nothing is meant to.

#![no_std]

pub mod air;
pub mod beacon;
pub mod dedup;
pub mod hci;
pub mod link;
pub mod outbox;
pub mod plan;
pub mod stall;

/// Re-exported so consumers can build link payloads without depending on
/// `heapless` themselves, and can never end up on a mismatched version.
pub use heapless;

pub use air::{AdminMsg, DecodeError, Frame, HeartbeatMsg, MsgType, SightingMsg};
pub use beacon::{Sighting, parse_mgmt};
pub use dedup::MacRing;
pub use hci::AdvReport;
pub use link::{BridgeToHost, HostToBridge, LinkError, SendStatus};
pub use outbox::{ByteSink, Outbox};
pub use plan::{ChannelPool, SCAN_CHANNELS};
pub use stall::StallWatch;
