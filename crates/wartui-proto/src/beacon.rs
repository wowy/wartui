//! Turning 802.11 management frames into observations.
//!
//! The vendor node discovers access points with `WiFi.scanNetworks(..., passive
//! = false, ...)` (`src/WiFiOps.cpp:745,755,779,3311`), so it transmits a probe
//! request on every channel it is assigned — including the DFS channels, where
//! the rules say listen and do not speak. A wartui node parks the radio and
//! listens instead, which means it has to do for itself the one thing the scan
//! API was buying: work out what a beacon is advertising.
//!
//! The firmware already contains that code. `getAuthType`
//! (`src/WiFiOps.cpp:154-408`) is a complete RSN/WPA information-element parser
//! that nothing in that tree calls — the scan path was there first and the
//! sniffing path was never finished. This module is a port of it, and of the
//! `wifi_auth_mode_t` table it feeds (`security_int_to_string`,
//! `src/WiFiOps.cpp:1833-1878`), collapsed into one pass that yields a
//! [`Security`] directly. The set of values has to stay faithful to that table
//! even though the spelling no longer travels on the wire: it is what the
//! exported WiGLE `AuthMode` column ends up saying.
//!
//! Parsing lives here, on the host side of the path dependency, because that is
//! where `cargo test` can reach it. A misread information element that only the
//! firmware knew about would cost a reflash to find and another to fix.

use crate::air::{RecordKind, SSID_MAX, Security, SightingMsg};

/// 802.11 MAC header length for a management frame: no QoS, no HT control.
const HDR_LEN: usize = 24;

/// Timestamp, beacon interval and capability info, ahead of the elements.
const FIXED_LEN: usize = 12;

/// `Capability Information` bit 4.
const CAP_PRIVACY: u16 = 0x0010;

/// One access point, as heard on the air.
///
/// Fixed-size and [`Copy`] on purpose: the firmware builds these inside the
/// promiscuous receive callback, where the frame buffer belongs to the Wi-Fi
/// driver and is gone the moment the callback returns, and parks them in a
/// `static` ring for the main loop to drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sighting {
    /// The BSSID, from `addr3` of the management header.
    pub bssid: [u8; 6],
    ssid: [u8; SSID_MAX],
    ssid_len: u8,
    /// The `AuthMode` token this frame's elements amount to.
    pub security: Security,
    /// The channel the access point says it is on, or the one we were parked on.
    pub channel: u8,
    /// Signal strength in dBm, as the receiver reported it.
    pub rssi: i8,
}

impl Sighting {
    /// The SSID bytes, which are whatever the access point beaconed and so may
    /// not be UTF-8. Empty for a hidden network.
    #[must_use]
    pub fn ssid(&self) -> &[u8] {
        &self.ssid[..usize::from(self.ssid_len)]
    }

    /// The same observation in the shape the wire carries.
    #[must_use]
    pub fn as_msg(&self) -> SightingMsg<'_> {
        SightingMsg {
            kind: RecordKind::Wifi,
            bssid: self.bssid,
            channel: self.channel,
            rssi: self.rssi,
            security: self.security,
            ssid: self.ssid(),
        }
    }
}

/// Whether a frame is worth handing to [`parse_mgmt`].
///
/// Split out because promiscuous mode delivers every frame on the channel and
/// the overwhelming majority are data, so a caller wants to reject them before
/// doing anything that costs — taking a lock, most of all. Type 0 is
/// management, and only these two subtypes carry the elements
/// (`src/WiFiOps.cpp:167-176`).
#[must_use]
pub const fn is_report(frame: &[u8]) -> bool {
    let Some(&fc0) = frame.first() else { return false };
    (fc0 >> 2) & 0x03 == 0 && matches!((fc0 >> 4) & 0x0F, 8 | 5)
}

/// Decode a beacon or probe response into a [`Sighting`].
///
/// `frame` is the whole 802.11 frame as promiscuous mode delivers it, starting
/// at the MAC header. `parked` is the channel the radio was tuned to, used when
/// the frame carries no channel of its own.
///
/// Returns `None` for anything that is not a beacon (subtype 8) or probe
/// response (subtype 5), and for a frame too short to hold the fixed fields.
/// Everything past that point is best-effort: a truncated or malformed element
/// ends the walk rather than discarding the access point, which is what
/// `getAuthType` does and is the right call for a frame that was already
/// received well enough to have a BSSID.
#[must_use]
pub fn parse_mgmt(frame: &[u8], rssi: i8, parked: u8) -> Option<Sighting> {
    if frame.len() < HDR_LEN + FIXED_LEN || !is_report(frame) {
        return None;
    }

    let mut bssid = [0u8; 6];
    bssid.copy_from_slice(&frame[16..22]);

    let capability = u16::from_le_bytes([frame[HDR_LEN + 10], frame[HDR_LEN + 11]]);
    let mut scan = Elements::new(capability & CAP_PRIVACY != 0);
    scan.walk(&frame[HDR_LEN + FIXED_LEN..]);

    Some(Sighting {
        bssid,
        ssid: scan.ssid,
        ssid_len: scan.ssid_len,
        security: scan.classify(),
        channel: scan.channel.unwrap_or(parked),
        rssi,
    })
}

/// What one walk over a frame's information elements found.
struct Elements {
    ssid: [u8; SSID_MAX],
    ssid_len: u8,
    channel: Option<u8>,
    privacy: bool,
    has_rsn: bool,
    has_wpa: bool,
    has_wapi: bool,
    rsn_psk: bool,
    rsn_8021x: bool,
    rsn_sae: bool,
    rsn_owe: bool,
    wpa_psk: bool,
    wpa_8021x: bool,
}

impl Elements {
    const fn new(privacy: bool) -> Self {
        Self {
            ssid: [0; SSID_MAX],
            ssid_len: 0,
            channel: None,
            privacy,
            has_rsn: false,
            has_wpa: false,
            has_wapi: false,
            rsn_psk: false,
            rsn_8021x: false,
            rsn_sae: false,
            rsn_owe: false,
            wpa_psk: false,
            wpa_8021x: false,
        }
    }

    /// Walk `tag, len, data` triples until they stop making sense.
    fn walk(&mut self, mut ies: &[u8]) {
        while ies.len() >= 2 {
            let (id, len) = (ies[0], usize::from(ies[1]));
            let Some(data) = ies.get(2..2 + len) else { break };
            match id {
                // SSID. Longer than 32 bytes is not a legal SSID; keep what
                // fits rather than dropping the access point over it.
                0 => {
                    let take = len.min(SSID_MAX);
                    self.ssid[..take].copy_from_slice(&data[..take]);
                    // Cast is safe: `take` is at most SSID_MAX, which is 32.
                    #[allow(clippy::cast_possible_truncation)]
                    {
                        self.ssid_len = take as u8;
                    }
                }
                // DS Parameter Set: the primary channel, on 2.4 GHz.
                //
                // First wins, here and for HT Operation below. Elements arrive
                // in ascending tag order so this is DS-then-HT anyway, and it
                // means a stray trailing element — four bytes of frame check
                // sequence read as `03 01 xx`, say — cannot rewrite a channel
                // the access point actually named.
                3 if len >= 1 && self.channel.is_none() => self.channel = Some(data[0]),
                // RSN.
                48 if len >= 8 => {
                    self.has_rsn = true;
                    self.read_suites(data, true);
                }
                // HT Operation, whose first byte is the primary channel. This
                // is how a 5 GHz access point says where it is: DS Parameter
                // Set is a 2.4 GHz element and 5 GHz beacons omit it.
                61 if len >= 1 && self.channel.is_none() => self.channel = Some(data[0]),
                // Vendor specific; WPA is Microsoft's OUI with type 1.
                221 if len >= 8 && data[..4] == [0x00, 0x50, 0xF2, 0x01] => {
                    self.has_wpa = true;
                    self.read_suites(&data[4..], false);
                }
                // WAPI. Its contents are not inspected, matching
                // `src/WiFiOps.cpp:329-332`.
                68 => self.has_wapi = true,
                _ => {}
            }
            ies = &ies[2 + len..];
        }
    }

    /// Skip the version, group cipher and pairwise list, then read the AKM
    /// suites. `src/WiFiOps.cpp:222-289` for RSN and `:277-325` for WPA; the
    /// two bodies are the same shape once the OUI and type are stripped.
    fn read_suites(&mut self, body: &[u8], rsn: bool) {
        // Version (2) + group cipher (4).
        let Some(rest) = body.get(6..) else { return };
        let Some(rest) = skip_suites(rest) else { return };
        let Some(akms) = take_suites(rest) else { return };

        let oui: [u8; 3] = if rsn { [0x00, 0x0F, 0xAC] } else { [0x00, 0x50, 0xF2] };
        for akm in akms.as_chunks::<4>().0 {
            if akm[..3] != oui {
                continue;
            }
            match (rsn, akm[3]) {
                // 802.1X, and its SHA-256 variant.
                (true, 1 | 5) => self.rsn_8021x = true,
                (true, 2 | 6) => self.rsn_psk = true,
                // SAE, and its fast-transition variant: WPA3.
                (true, 8 | 9) => self.rsn_sae = true,
                (true, 18) => self.rsn_owe = true,
                (false, 1) => self.wpa_8021x = true,
                (false, 2) => self.wpa_psk = true,
                _ => {}
            }
        }
    }

    /// The classification ladder from `src/WiFiOps.cpp:341-401`, mapped
    /// straight onto the tokens `security_int_to_string` would have produced.
    ///
    /// Order matters and is preserved verbatim. Note the two rungs that fall
    /// through to [`Security::Undefined`]: the firmware's `switch` has no arm
    /// for `WIFI_AUTH_OWE`, and none for WPA3-Enterprise, so both reach the
    /// WiGLE column as `[UNDEFINED]`. Reproducing that is deliberate — the
    /// column is a contract with an exporter, not a description of the network.
    fn classify(&self) -> Security {
        if self.has_wapi {
            return Security::WapiPsk;
        }
        if self.has_rsn && self.rsn_owe {
            return Security::Undefined;
        }
        if self.has_rsn && self.rsn_sae {
            return if self.rsn_psk { Security::Wpa2Wpa3Psk } else { Security::Wpa3Psk };
        }
        // `[WPA2]` is `WIFI_AUTH_WPA2_ENTERPRISE`, despite the spelling. A
        // WPA-only enterprise network lands here too: the firmware maps
        // `WIFI_AUTH_ENTERPRISE` to the same token.
        //
        // `!has_rsn` on the second arm is the vendor's, not a simplification of
        // it (`src/WiFiOps.cpp:375`). An access point advertising 802.1X in its
        // legacy WPA element and PSK in its RSN element is describing what it
        // will actually negotiate in the RSN element; without the guard it
        // reads as enterprise, and the two firmwares would disagree about the
        // same beacon.
        if (self.has_rsn && self.rsn_8021x) || (self.has_wpa && self.wpa_8021x && !self.has_rsn) {
            return Security::Wpa2Enterprise;
        }
        match (self.has_wpa && self.wpa_psk, self.has_rsn && self.rsn_psk) {
            (true, true) => return Security::WpaWpa2Psk,
            (false, true) => return Security::Wpa2Psk,
            (true, false) => return Security::WpaPsk,
            (false, false) => {}
        }
        // Nothing named a cipher suite, so the privacy bit is all there is to
        // go on, and WEP is what it used to mean.
        if self.privacy { Security::Wep } else { Security::Open }
    }
}

/// Skip a `count: u16le` followed by that many 4-byte suites.
fn skip_suites(body: &[u8]) -> Option<&[u8]> {
    let count = usize::from(u16::from_le_bytes([*body.first()?, *body.get(1)?]));
    body.get(2 + count.checked_mul(4)?..)
}

/// Read a `count: u16le` followed by that many 4-byte suites.
fn take_suites(body: &[u8]) -> Option<&[u8]> {
    let count = usize::from(u16::from_le_bytes([*body.first()?, *body.get(1)?]));
    body.get(2..2 + count.checked_mul(4)?)
}
