//! The headless half of wartui: what the fleet is doing, what gets written
//! down, and what comes back out.
//!
//! Nothing here draws a terminal or parses an argument. The split is what makes
//! `wartui export` and the TUI two front ends over one implementation, and what
//! lets the fleet's behaviour be tested without either.
//!
//! [`engine`] is a pure synchronous state machine, [`runtime`] owns the clock and
//! performs what it asks for, [`store`] is the system of record and [`export`] a view
//! of it. The only thing wartui transmits is a channel assignment, to one node, in
//! the 100 ms it holds open after a heartbeat.

pub mod distinct;
pub mod engine;
pub mod export;
pub mod gps;
pub mod nmea;
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
    BatchTiming, Checkpoint, CheckpointPass, CheckpointReport, SessionInfo, Store, StoreConfig,
    StoreError, StoreReport,
};
