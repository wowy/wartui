use core::fmt;

/// What a network's information elements amount to.
///
/// One byte on the wire. The WiGLE `AuthMode` spelling appears only where WiGLE wants
/// it, through [`Display`](fmt::Display), which the store row and the exported column
/// both use. Spelling it on the wire would cost up to fifteen bytes a record rather than
/// one, and make decoding a string comparison.
///
/// A discriminant this build does not know is carried as [`Security::Unknown`] rather
/// than failing the frame. A sighting must not be lost to a security mode this build
/// has no name for.
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
    /// A discriminant this build does not name.
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

    /// The inverse. Never yields [`Security::Unknown`] holding a value a named variant
    /// has, so `as_u8` and `from_u8` round-trip.
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
            // Still shaped like an AuthMode token, and still says which value it
            // was, so the export gives a lead rather than a shrug.
            None => write!(f, "[UNKNOWN:{}]", self.as_u8()),
        }
    }
}
