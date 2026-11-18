//! The WardriveESP protocol: the text a phone app reads Wi-Fi sightings in, over Bluetooth.
//!
//! WardriveESP is by RocketGod, MIT licensed; the document is vendored at
//! `docs/vendor/wardrive-esp/PROTOCOL.md`. Phone apps already speak it, so the bridge offers its
//! GATT service and a phone reads the fleet as it reads one WardriveESP board.
//!
//! The bridge implements the baseline Bluetooth part: Wi-Fi access points only. It takes commands
//! and sends the lines that start, describe, carry and stop a Wi-Fi scan, and answers every other
//! mode as unsupported. Its advertisements carry the status beacon and advert extras.
//!
//! | Module | What it does | Who runs it |
//! |---|---|---|
//! | [`frame`] | the GATT identity and the 12-byte frames around commands and output | the C6 |
//! | [`command`] | reads a command's text | the P4 |
//! | [`line`](mod@line) | writes each output line | the P4 |
//! | [`session`] | what a phone's session keeps, and the lines each event sends | the P4 |
//! | [`beacon`] | the status beacon and advert extras | the P4 fills it, the C6 encodes it |
//!
//! It lives here, pure and allocation-free, so `cargo test` covers it rather than a reflash.

pub mod beacon;
pub mod command;
pub mod frame;
pub mod line;
pub mod session;
