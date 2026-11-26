//! The headless app layer a front end binds to: the TUI on a desktop, and an app on a phone.
//!
//! It holds what a front end does with the operator's settings beyond drawing them: their schema
//! and rules, where they are stored, and the one path that writes them ([`settings`]). It words
//! nothing for the operator, so each front end says what happened in its own way.
//!
//! It compiles for a phone, so it names no desktop I/O library and no terminal. The desktop's
//! `wartui.toml` file is a [`settings::SettingsStore`] in `wartui-desktop`.

pub mod settings;
