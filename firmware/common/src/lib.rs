//! Chip-level code both wartui firmwares need.
//!
//! What belongs here is what the bridge and the node would otherwise each carry a copy of
//! and that needs `esp-hal` to say. Logic that needs no chip lives in `wartui-proto`,
//! where `cargo test` reaches it; this crate cannot live there, because `wartui-proto` is
//! a host workspace member built with `--all-features`, which would select both chips'
//! `esp-hal` features at once.
//!
//! Each firmware forwards its chip feature here, so one build selects one chip.
#![no_std]

use esp_hal::rtc_cntl::SocResetReason;
use wartui_proto::reset::ResetCause;

#[cfg(not(any(feature = "esp32c5", feature = "esp32c6")))]
compile_error!("select a chip: --features esp32c5 or --features esp32c6");
#[cfg(all(feature = "esp32c5", feature = "esp32c6"))]
compile_error!("select exactly one chip: esp32c5 and esp32c6 are mutually exclusive");

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

/// Reset the chip, undoing first what the C5's ROM leaves behind.
///
/// Every reset either firmware takes comes through here — both panic handlers, and on
/// the bridge its stall detector and `HostToBridge::Reset` — because on the C5 a bare
/// `software_reset()` does not reboot the board, it ends it. The part comes up
/// with `PCR.RESET_EVENT_BYPASS.reset_event_bypass` set, which keeps a core reset
/// from also resetting the system bus; the ROM's own MSPI core reset then leaves
/// the AXI bus frozen. The banner prints, `SPI mode:` never does, and nothing
/// recovers the board until it loses power — measured four ways. On the bridge that
/// is strictly worse than the wedge the stall detector exists to clear.
///
/// Clearing the bit is what ESP-IDF does on every boot and what `esp-hal` does in
/// its C5 `pre_init` from 1.2 onwards (esp-rs/esp-hal#5703), which the version
/// wall in `firmware/bridge/README.md` keeps out of reach. Written here rather than
/// in either `main` because a panic can land before `main` reaches a line of its own,
/// and one funnel is one place to delete when that pin moves — see issue #16.
pub fn reboot() -> ! {
    #[cfg(feature = "esp32c5")]
    esp_hal::peripherals::PCR::regs()
        .reset_event_bypass()
        .modify(|_, w| w.reset_event_bypass().clear_bit());

    esp_hal::system::software_reset()
}
