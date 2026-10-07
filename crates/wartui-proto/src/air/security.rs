use core::fmt;

/// What a network's information elements amount to.
///
/// One byte on the wire, and the WiGLE `AuthMode` spelling only at the edge
/// where WiGLE wants it — [`Display`](fmt::Display), which the store row and
/// the exported column both go through. Spelling it on the wire would
/// cost seventy bytes a record to carry a handful of values, and make the parser
/// on this end a string comparison.
///
/// The set is open-ended, so a discriminant this build does not know is carried
/// through as [`Security::Unknown`] rather than failing the frame: a node from
/// a later build must not lose an observation to a security mode this host has
/// never heard of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum Security {
    Open,
    Wep,
    WpaPsk,
    Wpa2Psk,
    WpaWpa2Psk,
    /// `[WPA2]`, which is `WIFI_AUTH_WPA2_ENTERPRISE` despite the name.
    Wpa2Enterprise,
    Wpa3Psk,
    Wpa2Wpa3Psk,
    WapiPsk,
    Undefined,
    /// The placeholder the BLE path reports.
    Ble,
    /// A discriminant this build did not have when it was written.
    Unknown(u8),
}

impl Security {
    /// The discriminant as it appears on the wire.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Open => 0,
            Self::Wep => 1,
            Self::WpaPsk => 2,
            Self::Wpa2Psk => 3,
            Self::WpaWpa2Psk => 4,
            Self::Wpa2Enterprise => 5,
            Self::Wpa3Psk => 6,
            Self::Wpa2Wpa3Psk => 7,
            Self::WapiPsk => 8,
            Self::Undefined => 9,
            Self::Ble => 10,
            Self::Unknown(raw) => raw,
        }
    }

    /// The inverse. Never yields [`Security::Unknown`] holding a value one of
    /// the named variants already has, so `as_u8` and `from_u8` round-trip.
    #[must_use]
    pub const fn from_u8(raw: u8) -> Self {
        match raw {
            0 => Self::Open,
            1 => Self::Wep,
            2 => Self::WpaPsk,
            3 => Self::Wpa2Psk,
            4 => Self::WpaWpa2Psk,
            5 => Self::Wpa2Enterprise,
            6 => Self::Wpa3Psk,
            7 => Self::Wpa2Wpa3Psk,
            8 => Self::WapiPsk,
            9 => Self::Undefined,
            10 => Self::Ble,
            other => Self::Unknown(other),
        }
    }

    /// The WiGLE `AuthMode` token, for everything but [`Security::Unknown`].
    #[must_use]
    pub const fn token(self) -> Option<&'static str> {
        Some(match self {
            Self::Open => "[OPEN]",
            Self::Wep => "[WEP]",
            Self::WpaPsk => "[WPA_PSK]",
            Self::Wpa2Psk => "[WPA2_PSK]",
            Self::WpaWpa2Psk => "[WPA_WPA2_PSK]",
            Self::Wpa2Enterprise => "[WPA2]",
            Self::Wpa3Psk => "[WPA3_PSK]",
            Self::Wpa2Wpa3Psk => "[WPA2_WPA3_PSK]",
            Self::WapiPsk => "[WAPI_PSK]",
            Self::Undefined => "[UNDEFINED]",
            Self::Ble => "[BLE]",
            Self::Unknown(_) => return None,
        })
    }
}

impl fmt::Display for Security {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.token() {
            Some(token) => f.write_str(token),
            // Said in a shape that still reads as an AuthMode token and still
            // says which one, so an export from an older host is a lead rather
            // than a shrug.
            None => write!(f, "[UNKNOWN:{}]", self.as_u8()),
        }
    }
}
