//! The headless half of wartui: what the fleet is doing, what gets written
//! down, and what comes back out.
//!
//! Nothing here draws a terminal or parses an argument. The split is what makes
//! `wartui export` and the TUI two front ends over one implementation, and what
//! lets the fleet's behaviour be tested without either.
//!
//! The shape is deliberate and load-bearing:
//!
//! - [`engine`] is a pure synchronous state machine. Events in, actions out, no
//!   I/O and no clock of its own.
//! - [`runtime`] owns the clock and performs those actions.
//! - [`store`] is the system of record; [`export`] is a view of it.
//!
//! This build never transmits. Everything below is receive-only, which is why
//! it can be pointed at a live fleet with no risk of disturbing it.

pub mod engine;
pub mod export;
pub mod position;
pub mod record;
pub mod runtime;
pub mod store;

pub use engine::{ActionBatch, Counters, Event, FleetEngine, NodeState, Now, Snapshot};
pub use position::{Fix, PositionChain, PositionSource};
pub use record::Record;
pub use store::{SessionInfo, Store, StoreConfig, StoreError};
