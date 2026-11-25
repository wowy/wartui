//! The desktop's adapters under the portable crates' seams: the host's serial ports
//! ([`serial`]), its Bluetooth radio ([`radio`]), BlueZ pairing on Linux (`bluez`) and the host's
//! own health ([`health`]).
//!
//! `wartui-bridge` and `wartui-core` compile for a phone, so they name no desktop I/O library.
//! They reach the OS through traits (`wartui_bridge::pipe::Ports`, `wartui_bridge::pipe::Radio`,
//! [`wartui_core::gps::ReceiverPorts`]) and an injected [`wartui_core::health::HealthReader`].
//! This is the one host crate that names `serialport`, `btleplug`, `dbus` or `libc`, and the
//! desktop front end (`wartui`) plugs it in.

#[cfg(target_os = "linux")]
pub mod bluez;
pub mod health;
pub mod radio;
pub mod serial;

pub use radio::{BleRadio, ble_radio, is_remembered, radio_traits, refused_for_bonding};
pub use serial::{SerialPorts, receiver_ports, serial_ports};
