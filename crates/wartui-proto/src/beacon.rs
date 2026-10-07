//! Turning 802.11 management frames into sightings.
//!
//! An active scan transmits a probe request on every channel it visits, including the
//! DFS channels, where the rules say listen and do not speak. A wartui node parks the
//! radio and listens instead. It must therefore do the one thing the scan API did for
//! it: work out what a beacon advertises.
//!
//! This module parses the RSN and WPA information elements and maps them onto the
//! `wifi_auth_mode_t` table in one pass, yielding a [`Security`]. The wire carries only
//! the discriminant, but the set of values must stay faithful to that table, because it
//! is what the WiGLE `AuthMode` column says.
//!
//! An access point hides its name in either of two ways: an SSID element of length zero,
//! or the element at the name's real length with every byte zero. Only the first looks
//! hidden. The second is a well-formed SSID made of NULs, and survives everything
//! downstream: `air` is byte-transparent, the store keeps what arrived, and NUL is valid
//! UTF-8. [`visible_ssid`] strips it here, once, so `ssid_len == 0` means hidden
//! everywhere.

use core::fmt;

use crate::air::{EXT_MAX, RecordKind, SSID_MAX, Security, SightingMsg};

/// 802.11 MAC header length for a management frame: no QoS, no HT control.
const HDR_LEN: usize = 24;

/// Timestamp, beacon interval and capability info, ahead of the elements.
const FIXED_LEN: usize = 12;

/// `Capability Information` bit 4.
const CAP_PRIVACY: u16 = 0x0010;

/// One access point, as heard on the air.
///
/// Fixed-size and [`Copy`] on purpose: the firmware builds these inside the
/// promiscuous receive callback, whose buffer dies when it returns, and parks them
/// in a `static` ring for the main loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sighting {
    /// The BSSID, from `addr3` of the management header.
    pub bssid: [u8; 6],
    ssid: [u8; SSID_MAX],
    ssid_len: u8,
    rcoi: [u8; EXT_MAX],
    rcoi_len: u8,
    /// The `AuthMode` token this frame's elements amount to.
    pub security: Security,
    /// The channel the access point says it is on, or the one we were parked on.
    pub channel: u8,
    /// Signal strength in dBm, as the receiver reported it.
    pub rssi: i8,
}

impl Sighting {
    /// An all-zero sighting, for filling a buffer before anything is heard.
    pub(crate) const BLANK: Self = Self {
        bssid: [0; 6],
        ssid: [0; SSID_MAX],
        ssid_len: 0,
        rcoi: [0; EXT_MAX],
        rcoi_len: 0,
        security: Security::Open,
        channel: 0,
        rssi: 0,
    };

    /// The SSID bytes, whatever the access point beaconed, so not necessarily UTF-8.
    /// Empty for a hidden network, in either form [`visible_ssid`] describes.
    #[must_use]
    pub fn ssid(&self) -> &[u8] {
        &self.ssid[..usize::from(self.ssid_len)]
    }

    /// The roaming consortium element's body, verbatim as the beacon carried it.
    ///
    /// Empty when the access point beaconed none, as most do not. Also empty when the
    /// element was longer than the wire carries: it is dropped whole rather than
    /// truncated, since a partly-arrived identifier is nobody's. [`rcoi_text`] reads the
    /// bytes.
    #[must_use]
    pub fn rcoi(&self) -> &[u8] {
        &self.rcoi[..usize::from(self.rcoi_len)]
    }

    /// The same sighting as the record the wire carries.
    #[must_use]
    pub fn as_msg(&self) -> SightingMsg<'_> {
        SightingMsg {
            kind: RecordKind::Wifi,
            bssid: self.bssid,
            channel: self.channel,
            rssi: self.rssi,
            security: self.security,
            ssid: self.ssid(),
            ext: self.rcoi(),
        }
    }
}

/// The name an access point beaconed, with a cloaked one's zero padding stripped.
/// Empty if there was nothing but padding.
///
/// Only trailing zeros are padding. An *interior* NUL is left alone: nothing here says
/// what it was meant to be.
#[must_use]
pub fn visible_ssid(bytes: &[u8]) -> &[u8] {
    match bytes.iter().rposition(|&b| b != 0) {
        Some(last) => &bytes[..=last],
        None => &[],
    }
}

/// The roaming consortium element's body as WiGLE's `RCOIs` column spells it: each
/// identifier in hex, separated by single spaces.
///
/// The body is a count byte, then a byte holding the first two identifiers' lengths in
/// its nibbles, then the identifiers. A third identifier, if any, is whatever is left. A
/// body whose claimed lengths do not fit renders as nothing rather than in part.
#[must_use]
pub fn rcoi_text(body: &[u8]) -> RcoiText<'_> {
    RcoiText(body)
}

/// The rendering [`rcoi_text`] returns.
///
/// A `Display` rather than a `String`, because this crate is `no_std` and
/// allocation-free. The host formats it into a string where it has one.
pub struct RcoiText<'a>(&'a [u8]);

impl fmt::Display for RcoiText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The count byte says how many identifiers the *operator* has; the
        // body's own layout says how many are here, and it is the layout that
        // gets rendered. First-wins rules belong to the walk, not the body.
        let Some(ois) = self.0.get(2..) else { return Ok(()) };
        let lengths = self.0[1];
        let len1 = usize::from(lengths & 0x0F);
        let len2 = usize::from(lengths >> 4);
        if len1 + len2 > ois.len() {
            return Ok(());
        }
        let len3 = ois.len() - len1 - len2;
        let mut first = true;
        for (start, len) in [(0, len1), (len1, len2), (len1 + len2, len3)] {
            if len == 0 {
                continue;
            }
            if !first {
                f.write_str(" ")?;
            }
            first = false;
            write_oi(f, &ois[start..start + len])?;
        }
        Ok(())
    }
}

/// One identifier: six hex digits for the three-byte form and ten for the five-byte
/// one, the two forms Hotspot 2.0 uses. Any other length is spelled out byte by byte
/// rather than padded into a shape it is not.
fn write_oi(f: &mut fmt::Formatter<'_>, oi: &[u8]) -> fmt::Result {
    match oi {
        [a, b, c] => write!(f, "{:06X}", u32::from_be_bytes([0, *a, *b, *c])),
        [a, b, c, d, e] => {
            write!(f, "{:010X}", u64::from_be_bytes([0, 0, 0, *a, *b, *c, *d, *e]))
        }
        _ => oi.iter().try_for_each(|byte| write!(f, "{byte:02X}")),
    }
}

/// Whether a frame is worth handing to [`parse_mgmt`].
///
/// Promiscuous mode delivers every frame on the channel, and most are data. This lets a
/// caller reject them before anything that costs, above all taking a lock. It passes
/// beacons and probe responses, the two management subtypes that carry the elements.
#[must_use]
pub const fn is_report(frame: &[u8]) -> bool {
    let Some(&fc0) = frame.first() else { return false };
    (fc0 >> 2) & 0x03 == 0 && matches!((fc0 >> 4) & 0x0F, 8 | 5)
}

/// Decode a beacon or probe response into a [`Sighting`].
///
/// `frame` is the whole 802.11 frame as promiscuous mode delivers it, from the MAC
/// header on. `parked` is the channel the radio was tuned to, used when the frame names
/// no channel of its own.
///
/// Returns `None` for anything but a beacon (subtype 8) or probe response (subtype 5),
/// and for a frame too short for the fixed fields. Past that it is best-effort. A
/// malformed element ends the walk rather than discarding an access point whose BSSID
/// arrived intact.
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
        rcoi: scan.rcoi,
        rcoi_len: scan.rcoi_len,
        security: scan.classify(),
        channel: scan.channel.unwrap_or(parked),
        rssi,
    })
}

/// What one walk over a frame's information elements found.
struct Elements {
    ssid: [u8; SSID_MAX],
    ssid_len: u8,
    rcoi: [u8; EXT_MAX],
    rcoi_len: u8,
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
            rcoi: [0; EXT_MAX],
            rcoi_len: 0,
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
                // SSID. Longer than 32 bytes is not legal; keep what fits rather
                // than drop the access point. Clamp first, then trim. Trimming first
                // turns an over-long element whose only non-zero byte is past the
                // 32nd into a name made of padding, rather than a hidden network.
                0 => {
                    let take = visible_ssid(&data[..len.min(SSID_MAX)]).len();
                    self.ssid[..take].copy_from_slice(&data[..take]);
                    // Cast is safe: `take` is at most SSID_MAX, which is 32.
                    #[allow(clippy::cast_possible_truncation)]
                    {
                        self.ssid_len = take as u8;
                    }
                }
                // DS Parameter Set: the primary channel, on 2.4 GHz.
                //
                // First wins, here and for HT Operation below. Elements arrive in
                // ascending tag order, so this is DS then HT anyway. It also stops a
                // stray trailing element, such as the frame check sequence read as
                // `03 01 xx`, from rewriting a channel the access point named.
                3 if len >= 1 && self.channel.is_none() => self.channel = Some(data[0]),
                // RSN.
                48 if len >= 8 => {
                    self.has_rsn = true;
                    self.read_suites(data, true);
                }
                // Roaming Consortium: which hotspot operators a Passpoint access
                // point says will authenticate you. Kept verbatim, count and lengths
                // bytes included. The walk finds it; `rcoi_text` reads it on the
                // host, where a fix costs a re-export rather than a reflash.
                //
                // Dropped whole when longer than the wire carries, because part of
                // an identifier says nothing. First wins, like the channel elements.
                111 if (2..=EXT_MAX).contains(&len) && self.rcoi_len == 0 => {
                    self.rcoi[..len].copy_from_slice(data);
                    // Cast is safe: `len` is at most EXT_MAX, which is 17.
                    #[allow(clippy::cast_possible_truncation)]
                    {
                        self.rcoi_len = len as u8;
                    }
                }
                // HT Operation, whose first byte is the primary channel. A 5 GHz
                // beacon names its channel here, since it omits DS Parameter Set.
                61 if len >= 1 && self.channel.is_none() => self.channel = Some(data[0]),
                // Vendor specific; WPA is Microsoft's OUI with type 1.
                221 if len >= 8 && data[..4] == [0x00, 0x50, 0xF2, 0x01] => {
                    self.has_wpa = true;
                    self.read_suites(&data[4..], false);
                }
                // WAPI. Its contents are not inspected.
                68 => self.has_wapi = true,
                _ => {}
            }
            ies = &ies[2 + len..];
        }
    }

    /// Skip the version, group cipher and pairwise list, then read the AKM suites.
    /// RSN and WPA bodies are the same shape once the OUI and type are stripped.
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

    /// The classification ladder, mapped straight onto WiGLE's `AuthMode` tokens.
    ///
    /// Order matters, including the two rungs that fall through to
    /// [`Security::Undefined`]. The auth-mode table has no token for `WIFI_AUTH_OWE` or
    /// WPA3-Enterprise, so both reach the WiGLE column as `[UNDEFINED]`. The column is a
    /// contract with an exporter, not a description of the network.
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
        // `[WPA2]` is `WIFI_AUTH_WPA2_ENTERPRISE` despite the spelling, and a
        // WPA-only enterprise network lands here too.
        //
        // `!has_rsn` on the second arm is deliberate. An access point advertising
        // 802.1X in its legacy WPA element and PSK in its RSN element negotiates the
        // RSN one.
        if (self.has_rsn && self.rsn_8021x) || (self.has_wpa && self.wpa_8021x && !self.has_rsn) {
            return Security::Wpa2Enterprise;
        }
        match (self.has_wpa && self.wpa_psk, self.has_rsn && self.rsn_psk) {
            (true, true) => return Security::WpaWpa2Psk,
            (false, true) => return Security::Wpa2Psk,
            (true, false) => return Security::WpaPsk,
            (false, false) => {}
        }
        // Nothing named a cipher suite, so the privacy bit is all there is, and on
        // its own it means WEP.
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
