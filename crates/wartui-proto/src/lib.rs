//! Wire formats for the wartui ESP32-C5 wardriver mesh, the decision logic both
//! firmwares run, and the text form of the addresses that cross the wire.
//!
//! The host, the bridge firmware and the node firmware all compile this crate. Defining
//! the wire types once keeps the three in step. The formats are wartui's own, except
//! [`wardrive_esp`], the text protocol the bridge speaks to a phone app.
//!
//! The crate is `no_std` and allocation-free so the firmware can use it. Logic the
//! firmware runs lives here too, such as [`pending`] and [`ota`],
//! because a `riscv32imac` binary cannot run a test and this crate can.
//! [`config`] parses build-time buffer sizes, [`frame_ring`] holds whole frames in a bounded
//! ring, and [`host_queue`] queues link frames for the P4's host link, which [`host_owner`] says
//! whose it is: USB's, a Bluetooth central's, a phone's or no one's. [`radio_link`] is the
//! link between a P4 bridge and the board's C6, and [`split`] holds the P4's
//! decisions about that C6. [`ble`] is the GATT identity of the C6's Bluetooth host link.
//! [`bus`] frames the SPI transfers between a P4 bridge and its nodes, and holds the messages a
//! node and the P4 exchange over it. [`carrier`] reads the carrier
//! board's I/O expander at the P4's boot, and watches for a lost node supply. [`fan`] is the
//! carrier's lid fan: its duty, its PWM wave and tach reading. [`settings`] lays out the records
//! the P4 keeps its settings in, and [`bonds`] the Bluetooth bonds it keeps for its C6, each record
//! ending in a [`crc`] and both logged in a sector as [`sector_log`] lays out. [`fleet`] is the P4's fleet planner: who scans what, and the records that
//! tell each node. [`sd`] is the FAT32 volume the P4 formats its microSD card with, and the
//! reading of one it did not format. [`tsens`] converts the P4's temperature sensor's code.

#![no_std]

pub mod beacon;
pub mod ble;
pub mod bonds;
pub mod bus;
pub mod carrier;
pub mod config;
pub mod crc;
pub mod dedup;
pub mod fan;
pub mod first_flash;
pub mod fleet;
pub mod frame_ring;
pub mod hci;
pub mod host_owner;
pub mod host_queue;
pub mod link;
pub mod mac;
mod mac_index;
pub mod node;
pub mod ota;
pub mod pending;
pub mod plan;
pub mod radio_link;
pub mod reset;
pub mod sd;
pub mod sector_log;
pub mod settings;
pub mod split;
pub mod tsens;
pub mod wardrive_esp;

/// Re-exported so consumers can build link payloads without depending on
/// `heapless` themselves, and can never end up on a mismatched version.
pub use heapless;

pub use beacon::{Sighting, parse_mgmt};
pub use bus::{
    Assign, DecodeError, DownMsg, Heartbeat, NodeBus, PanicMsg, PanicStr, SightingBatch,
    SightingBatchWriter, SightingMsg, Slot, UpMsg,
};
pub use dedup::MacRing;
pub use hci::AdvReport;
pub use link::{BridgeToHost, HostToBridge, LinkError, Undelivered};
pub use plan::{ChannelPool, SCAN_CHANNELS};
