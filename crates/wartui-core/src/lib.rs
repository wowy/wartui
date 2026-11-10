//! The headless half of wartui: what the fleet does, what gets written down, and what comes back
//! out.
//!
//! Nothing here draws a terminal or parses an argument, so `wartui export` and the TUI are two
//! front ends over one implementation, and the fleet's behavior is testable without either.
//!
//! [`engine`] is a pure state machine, [`runtime`] owns the clock and does what it asks, [`store`]
//! is the system of record, and [`export`] and [`analyze`] are views of it.
//!
//! The P4 bridge plans the fleet and sends every node its assignment (`wartui_proto::fleet`); the
//! host asks it to start and stop, and mirrors what it reports. The only records the host sends a
//! node are an update's ([`ota`]), and a node that has accepted one services the bus continuously
//! until it ends.

pub mod analyze;
pub mod discover;
pub mod distinct;
pub mod engine;
pub mod export;
pub mod gps;
pub mod health;
pub mod nmea;
pub mod ota;
pub mod position;
pub mod record;
pub mod runtime;
pub mod store;
pub mod ubx;

pub use engine::{ActionBatch, Command, Counters, Event, FleetEngine, NodeState, Now, Snapshot};
pub use gps::{Gps, GpsConfig, GpsStatus, GpsView};
pub use position::{Fix, PositionChain, PositionSource};
pub use record::Record;
pub use store::{
    BatchTiming, CaptureInfo, Checkpoint, CheckpointPass, CheckpointReport, Store, StoreConfig,
    StoreError, StoreReport,
};
