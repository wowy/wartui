//! What only a desktop does, under the portable crates: the host's serial ports ([`serial`]), the
//! puck reader and its receiver search ([`gps`], [`discover`]), the Bluetooth radio ([`radio`]),
//! BlueZ pairing on Linux (`bluez`), the host's own health ([`health`]) and the `wartui.toml` file
//! the settings are kept in ([`config`]).
//!
//! `wartui-bridge`, `wartui-core` and `wartui-app` compile for a phone, so they name no desktop I/O
//! library. They reach the OS through traits (`wartui_bridge::pipe::Ports`,
//! `wartui_bridge::pipe::Radio`, [`wartui_app::settings::SettingsStore`]) and an injected
//! [`wartui_core::health::HealthReader`], and a host port's GPS reader drives a
//! [`wartui_core::Gps`] from outside. This is the one host crate that names `serialport`,
//! `btleplug`, `dbus` or `libc`, and the desktop front end (`wartui`) plugs it in.

#[cfg(target_os = "linux")]
pub mod bluez;
pub mod config;
pub mod discover;
pub mod gps;
pub mod health;
pub mod radio;
pub mod serial;

pub use radio::{BleRadio, ble_radio, is_remembered, radio_traits, refused_for_bonding};
pub use serial::{SerialPorts, serial_ports};
