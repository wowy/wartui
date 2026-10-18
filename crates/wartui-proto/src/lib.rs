//! Wire formats for the wartui ESP32-C5/C6 wardriver mesh, the decision logic both
//! firmwares run, and the text form of the addresses that cross the wire.
//!
//! The host, the bridge firmware and the node firmware all compile this crate. Defining
//! the wire types once keeps the three in step. The formats are wartui's own and
//! interoperate with no other firmware.
//!
//! The crate is `no_std` and allocation-free so the firmware can use it. Logic the
//! firmware runs lives here too, such as [`outbox`], [`stall`], [`pending`] and [`ota`],
//! because a `riscv32imac` binary cannot run a test and this crate can.
//! [`config`] parses build-time buffer sizes, [`frame_ring`] holds whole frames in a bounded
//! ring, and [`host_queue`] queues link frames for the P4's USB host port. [`spi`] is the
//! link between a P4 bridge and the C6 that runs its radio.

#![no_std]

pub mod air;
pub mod beacon;
pub mod config;
pub mod dedup;
pub mod frame_ring;
pub mod hci;
pub mod host_queue;
pub mod link;
pub mod mac;
mod mac_index;
pub mod node;
pub mod ota;
pub mod outbox;
pub mod pending;
pub mod plan;
pub mod reset;
pub mod spi;
pub mod stall;
pub mod tx_power;

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
