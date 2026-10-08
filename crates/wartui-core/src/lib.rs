//! The headless half of wartui: what the fleet does, what gets written down, and what comes back
//! out.
//!
//! Nothing here draws a terminal or parses an argument, so `wartui export` and the TUI are two
//! front ends over one implementation, and the fleet's behavior is testable without either.
//!
//! [`engine`] is a pure state machine, [`runtime`] owns the clock and does what it asks, [`store`]
//! is the system of record, and [`export`] and [`analyze`] are views of it. [`panel`] views a
//! snapshot instead. It lives here because it is formatting with a rule in it, and rules belong
//! where `cargo test` reaches.
//!
//! wartui transmits to a node only in the 100 ms admin window it holds open after a heartbeat:
//! channel assignments and dedup-ring clears.

pub mod analyze;
pub mod discover;
pub mod distinct;
pub mod engine;
pub mod export;
pub mod gps;
pub mod health;
pub mod nmea;
pub mod panel;
pub mod position;
pub mod record;
pub mod runtime;
pub mod store;

pub use engine::{
    ActionBatch, Assignment, Command, Counters, Event, FleetEngine, NodeState, Now, Snapshot,
};
pub use gps::{Gps, GpsConfig, GpsStatus, GpsView};
pub use position::{Fix, PositionChain, PositionSource};
pub use record::{AdminOutcome, Record};
pub use store::{
    BatchTiming, CaptureInfo, Checkpoint, CheckpointPass, CheckpointReport, Store, StoreConfig,
    StoreError, StoreReport,
};
