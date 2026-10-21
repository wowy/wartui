//! Chip-level code the wartui firmwares need.
//!
//! What belongs here is what two firmwares would otherwise each carry a copy of and that
//! needs `esp-hal` to say. Logic that needs no chip lives in `wartui-proto`,
//! where `cargo test` reaches it; this crate cannot live there, because `wartui-proto` is
//! a host workspace member built with `--all-features`, which would select both chips'
//! `esp-hal` features at once.
//!
//! Each firmware forwards its chip feature here, so one build selects one chip. Only the
//! node uses [`health`]. [`phase`] is a bridge's, whatever chip it runs on. [`radio`] is the
//! ESP-NOW half of a bridge, behind the `radio` feature, so a firmware whose chip has a
//! radio and bridges it carries the one copy; the P4 has no radio and cannot select it.
//! [`gps`] reads a u-blox receiver on a bridge's I2C bus, behind the `gps` feature, on any
//! of the three chips. [`ota`] is updates on flash, behind the `ota` feature, for the node and
//! the P4 bridge's C6.
#![no_std]

#[cfg(feature = "gps")]
pub mod gps;
pub mod health;
#[cfg(feature = "ota")]
pub mod ota;
pub mod phase;
#[cfg(feature = "radio")]
pub mod radio;

use esp_hal::rtc_cntl::SocResetReason;
use wartui_proto::reset::ResetCause;

#[cfg(not(any(feature = "esp32c5", feature = "esp32c6", feature = "esp32p4")))]
compile_error!("select a chip: --features esp32c5, esp32c6 or esp32p4");
// Counted rather than checked pairwise, so a fourth chip adds a term rather than a clause
// per existing one.
const _: () = assert!(
    cfg!(feature = "esp32c5") as usize
        + cfg!(feature = "esp32c6") as usize
        + cfg!(feature = "esp32p4") as usize
        <= 1,
    "select exactly one chip: esp32c5, esp32c6 and esp32p4 are mutually exclusive"
);
#[cfg(all(feature = "radio", feature = "esp32p4"))]
compile_error!("the ESP32-P4 has no radio; build `radio` with esp32c5 or esp32c6");
#[cfg(all(feature = "ota", feature = "esp32p4"))]
compile_error!("the ESP32-P4 takes no update over the air; build `ota` with esp32c5 or esp32c6");

/// Flatten the chip's reset reason into the handful of stories worth telling.
///
/// `SocResetReason` names silicon blocks rather than causes, and the two chips
/// disagree about which blocks exist: the C5 has `PowerGlitch` and `CpuLockup`
/// alone.
///
/// The numeric codes behind the shared names are identical on both parts, so the
/// temptation to match on `reason as u8` and be done should be resisted: the enum
/// is the only thing that makes the next chip's differences visible. Matching only
/// the variants every chip defines silently costs the C5 both of its own.
///
/// What is left unmapped is deliberate. `CoreDeepSleep` cannot happen: neither
/// firmware sleeps. `CoreSDIO` and `CoreEfuseCrc` say nothing an operator could act on
/// beyond "this board is unwell", which [`ResetCause::Unknown`] already says.
#[must_use]
pub fn reset_cause() -> ResetCause {
    let Some(reason) = esp_hal::system::reset_reason() else {
        return ResetCause::Unknown;
    };
    match reason {
        SocResetReason::ChipPowerOn => ResetCause::PowerOn,
        // Every panic handler arrives here through `reboot`, and so do the bridge's
        // `HostToBridge::Reset` and the reset its `StallWatch` asks for. Which of
        // those it was is what the bridge's phase marker is for.
        SocResetReason::CoreSw | SocResetReason::Cpu0Sw => ResetCause::Software,
        SocResetReason::CoreMwdt0
        | SocResetReason::CoreMwdt1
        | SocResetReason::CoreRtcWdt
        | SocResetReason::Cpu0Mwdt0
        | SocResetReason::Cpu0Mwdt1
        | SocResetReason::Cpu0RtcWdt
        | SocResetReason::SysRtcWdt
        | SocResetReason::SysSuperWdt => ResetCause::Watchdog,
        // A glitch on the supply rail is a brownout to anyone holding the board,
        // and the remedy printed for it is the right one.
        #[cfg(feature = "esp32c5")]
        SocResetReason::PowerGlitch => ResetCause::Brownout,
        // The only signal either part gives for the hang class, and only the C5
        // gives it — with no working watchdog behind it, a C6 simply does not
        // report that class at all.
        #[cfg(feature = "esp32c5")]
        SocResetReason::CpuLockup => ResetCause::Lockup,
        SocResetReason::SysBrownOut => ResetCause::Brownout,
        // `espflash reset` drives this pair over DTR/RTS, so an operator who
        // reached for the tool sees that they did.
        SocResetReason::CoreUsbUart | SocResetReason::CoreUsbJtag | SocResetReason::Cpu0JtagCpu => {
            ResetCause::External
        }
        _ => ResetCause::Unknown,
    }
}

/// Reset the chip: the one funnel every reset either firmware takes.
///
/// Both panic handlers come here, and on the bridge its stall detector and
/// `HostToBridge::Reset`. On the C5 a bare `software_reset()` reboots the board only
/// once `esp_hal::init` has run. The part comes up with
/// `PCR.RESET_EVENT_BYPASS.reset_event_bypass` set, which keeps a core reset from also
/// resetting the system bus; the ROM's own MSPI core reset then leaves the AXI bus
/// frozen. The banner prints, `SPI mode:` never does, and nothing recovers the board
/// until it loses power — measured four ways (`docs/phase-3-findings.md`). On the bridge
/// that is strictly worse than the wedge the stall detector exists to clear.
///
/// `esp-hal` clears the bit in its C5 `pre_init`, the first thing `esp_hal::init` does
/// and before anything in it can panic (esp-rs/esp-hal#5703), as ESP-IDF's bootloader
/// does. One funnel keeps every reset on the path that was benched, and is the one
/// place to change if a chip needs more.
pub fn reboot() -> ! {
    esp_hal::system::software_reset()
}
