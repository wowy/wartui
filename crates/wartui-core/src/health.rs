//! The host's own health: firmware throttling, SoC temperature and battery.
//!
//! The capture host is a HackberryPi CM5 (a Raspberry Pi CM5) in a car, on its own battery and
//! usually plugged in too. A battery or charger sagging under load and a SoC cooking in a parked
//! car are what can go wrong with the host, and nothing else in a capture shows them.
//!
//! `throttled` is the Raspberry Pi firmware's `get_throttled` word, as `vcgencmd get_throttled`
//! prints it. `analyze` reads these bits:
//!
//! | bit   | meaning                               |
//! |-------|---------------------------------------|
//! | 0     | under-voltage now                     |
//! | 1     | ARM frequency capped now              |
//! | 2     | throttled now                         |
//! | 3     | soft temperature limit now            |
//! | 16–19 | the same four, at any time since boot |
//!
//! **Core reads none of these.** Each is the OS's, and this crate compiles for a phone, so a front
//! end injects a [`HealthReader`] into [`crate::runtime::drive`]. `wartui-desktop`'s `health` is
//! the Raspberry Pi one. With no reader, every row records [`Health::default`].

/// One reading of the host's health. Every field is `None` where it cannot be read.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Health {
    /// The Raspberry Pi firmware's `get_throttled` word (bits in the module docs). `None` off a Pi.
    pub throttled: Option<u32>,
    /// Thermal zone 0, in thousandths of a degree Celsius.
    pub soc_temp_mc: Option<i32>,
    /// Battery voltage in millivolts, from the hwmon device named `battery`.
    pub battery_mv: Option<i32>,
    /// Battery current in milliamps, from the same device, signed as its driver reports it.
    pub battery_ma: Option<i32>,
}

/// What reads the host's health, once per host sample ([`crate::runtime::HOST_SAMPLE`]). A front
/// end supplies it, since every value is the OS's.
pub type HealthReader = Box<dyn FnMut() -> Health + Send>;
