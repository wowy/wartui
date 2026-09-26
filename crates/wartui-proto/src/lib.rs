//! Wire formats for the ESP32-C5 wardriver mesh.
//!
//! This crate is `no_std` and allocation-free because it is compiled into both
//! the host TUI and the bridge firmware. Defining the wire types once is the
//! only thing that keeps the two ends from drifting apart.
//!
//! The wire formats are wartui's own, and nothing here interoperates with any other
//! firmware.

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

pub use air::{
    AdminMsg, DecodeError, Frame, HeartbeatMsg, MsgType, SightingBatch, SightingBatchWriter,
    SightingMsg,
};
pub use beacon::{Sighting, parse_mgmt};
pub use dedup::MacRing;
pub use hci::AdvReport;
pub use link::{BridgeToHost, HostToBridge, LinkError, SendStatus};
pub use outbox::{ByteSink, Outbox};
pub use plan::{ChannelPool, SCAN_CHANNELS};
pub use stall::StallWatch;
