//! The headless app layer a front end binds to: the TUI on a desktop, and an app on a phone.
//!
//! It holds what a front end does beyond drawing: the operator's settings, their rules, where
//! they are stored and the one path that writes them ([`settings`]), and the capture it runs,
//! assembled, followed and stopped the same way whatever starts it ([`session`]), what it asks
//! a bridge outside a capture: its status, a setting, the pairing window, a power-cycle ([`ops`]),
//! where its rows are placed, the position chain's order and the wait for a fix ([`position`]),
//! what a fleet view shows of a capture's snapshot, as plain data ([`view`]), a capture sent
//! to WDGWars and WiGLE once the operator has said yes ([`upload`]), and the nodes and the
//! bridge updated over the link, from images checked before they are sent ([`update`]).
//! It words nothing for the operator, so each front end says what happened in its own way.
//!
//! It compiles for a phone, so it names no desktop I/O library and no terminal. The desktop's
//! `wartui.toml` file is a [`settings::SettingsStore`] in `wartui-desktop`.

pub mod ops;
pub mod position;
pub mod session;
pub mod settings;
pub mod update;
pub mod upload;
pub mod view;
