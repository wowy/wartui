//! Why a board is running this boot: the bridge reports it in
//! [`BridgeToHost::Ready`](crate::link::BridgeToHost::Ready), and a node in every
//! [`HeartbeatMsg`](crate::air::HeartbeatMsg).

use core::fmt;

use serde::Serialize;

/// Why a board, bridge or node, is running this boot rather than the last one.
///
/// A flattening of `esp_hal`'s per-chip `SocResetReason`, which names silicon blocks
/// rather than causes and differs between the two parts. An operator needs to know
/// which story this was, and the ones that matter are not [`Self::PowerOn`] or
/// [`Self::External`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum ResetCause {
    /// The board was plugged in, or the button was pressed.
    PowerOn,
    /// The firmware reset itself: the panic handler, or a bridge's
    /// [`HostToBridge::Reset`](crate::link::HostToBridge::Reset).
    Software,
    /// A watchdog fired, so the main loop stopped turning over.
    Watchdog,
    /// The CPU locked up and the silicon reset it.
    ///
    /// Reported by the C5 alone. Not folded into [`ResetCause::Watchdog`], because no
    /// watchdog on these parts fires (`docs/phase-3-findings.md`). On a C6 a hang has
    /// *no* signal, and the board must be unplugged.
    Lockup,
    /// The supply sagged. Usually a hub, a cable or a supply rather than the board.
    Brownout,
    /// A reset the firmware did not ask for and cannot attribute, including the one
    /// `espflash` drives over DTR/RTS.
    External,
    /// The chip reported something this build does not have a name for.
    Unknown,
}

impl ResetCause {
    /// The cause's byte in a [`HeartbeatMsg`](crate::air::HeartbeatMsg).
    ///
    /// [`Self::Unknown`] is 0, so a zeroed byte reads as nothing known, and the named
    /// causes count up from 1 in declaration order. The codes are fixed: a new cause
    /// takes the next free byte.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::PowerOn => 1,
            Self::Software => 2,
            Self::Watchdog => 3,
            Self::Lockup => 4,
            Self::Brownout => 5,
            Self::External => 6,
        }
    }

    /// The cause a heartbeat's byte names. A byte this build has no name for reads as
    /// [`Self::Unknown`] rather than failing the decode, so a node naming a newer cause
    /// is still a node.
    #[must_use]
    pub const fn from_u8(b: u8) -> Self {
        match b {
            1 => Self::PowerOn,
            2 => Self::Software,
            3 => Self::Watchdog,
            4 => Self::Lockup,
            5 => Self::Brownout,
            6 => Self::External,
            _ => Self::Unknown,
        }
    }

    /// The cause's lowercase snake name, as the store and the logs write it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PowerOn => "power_on",
            Self::Software => "software",
            Self::Watchdog => "watchdog",
            Self::Lockup => "lockup",
            Self::Brownout => "brownout",
            Self::External => "external",
            Self::Unknown => "unknown",
        }
    }

    /// Whether the board failed rather than being reset on purpose: a power-on or a reset
    /// over USB is the operator's own doing, and anything else (a panic, a watchdog, a
    /// lockup, a brownout, or a cause this build cannot name) is a fault worth raising.
    #[must_use]
    pub const fn is_fault(self) -> bool {
        match self {
            Self::PowerOn | Self::External => false,
            Self::Software | Self::Watchdog | Self::Lockup | Self::Brownout | Self::Unknown => true,
        }
    }

    /// Whether a bridge writes to USB before any host has spoken.
    ///
    /// It does iff a host was present when the previous life ended
    /// (`host_was_present`, kept in RTC memory). Never after a [`Self::PowerOn`], where
    /// that memory is garbage.
    ///
    /// - **Why not always.** Writing to the USB Serial/JTAG endpoint with no host
    ///   reading wedges it. A C6 replugged and left unread for two minutes was dead at
    ///   first open in 3 of 3 trials, until `StallWatch` rebooted it. A build holding all
    ///   transmit until a host frame decoded was healthy in 3 of 3, answering in 2 ms
    ///   (`docs/phase-3-findings.md`). The ROM banner prints either way and is not the
    ///   cause.
    /// - **Why not never.** A host sends one `Identify` per connection. A connection
    ///   that rides through the reset hears the new life only through the unprompted
    ///   `Ready`.
    /// - **Why not by cause.** The cause says who asked for the reset, not whether
    ///   anybody was reading. A panic or `StallWatch` reset after the host left is
    ///   [`Self::Software`] with nobody there. A watchdog or lockup reset can land while
    ///   a host keeps the port open and never sends again.
    #[must_use]
    pub const fn speaks_first(self, host_was_present: bool) -> bool {
        match self {
            Self::PowerOn => false,
            Self::Software
            | Self::Watchdog
            | Self::Lockup
            | Self::Brownout
            | Self::External
            | Self::Unknown => host_was_present,
        }
    }
}

impl fmt::Display for ResetCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::ResetCause;

    #[test]
    fn reset_cause_round_trips_byte_when_every_cause_encoded() {
        let causes = [
            ResetCause::Unknown,
            ResetCause::PowerOn,
            ResetCause::Software,
            ResetCause::Watchdog,
            ResetCause::Lockup,
            ResetCause::Brownout,
            ResetCause::External,
        ];
        for (code, cause) in causes.into_iter().enumerate() {
            assert_eq!(usize::from(cause.as_u8()), code);
            assert_eq!(ResetCause::from_u8(cause.as_u8()), cause);
        }
    }

    #[test]
    fn reset_cause_is_fault_unless_operator_reset_it_when_each_cause_checked() {
        assert!(!ResetCause::PowerOn.is_fault());
        assert!(!ResetCause::External.is_fault());
        assert!(ResetCause::Software.is_fault());
        assert!(ResetCause::Watchdog.is_fault());
        assert!(ResetCause::Lockup.is_fault());
        assert!(ResetCause::Brownout.is_fault());
        assert!(ResetCause::Unknown.is_fault());
    }

    #[test]
    fn reset_cause_reads_unknown_when_byte_unassigned() {
        assert_eq!(ResetCause::from_u8(7), ResetCause::Unknown);
        assert_eq!(ResetCause::from_u8(0xFF), ResetCause::Unknown);
    }
}
