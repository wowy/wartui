//! Turning 802.11 management frames into observations.
//!
//! An active scan transmits a probe request on every channel it visits — including
//! the DFS channels, where the rules say listen and do not speak. A wartui node
//! parks the radio and listens instead, which means it has to do for itself the
//! one thing the scan API was buying: work out what a beacon is advertising.
//!
//! An RSN/WPA information-element parser and the `wifi_auth_mode_t` table it feeds,
//! collapsed into one pass yielding a [`Security`]. The set of values has to stay
//! faithful to that table even though the spelling never travels on the wire — a
//! frame carries the discriminant as one byte — because it is what the WiGLE
//! `AuthMode` column says.
//!
//! [`visible_ssid`] exists because an access point hides its name in either of two
//! ways and only one of them looks hidden: an SSID element of length zero, or the
//! element at the name's real length with every byte zero. The second form is a
//! well-formed SSID made of NULs and survives everything downstream — `air` is
//! byte-transparent, the store keeps what arrived, and NUL is valid UTF-8 — so it
//! reached a WiGLE export as a column of them. Stripping it here rather than at each
//! place that shows an SSID is what makes `ssid_len == 0` mean hidden.

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
    /// The SSID bytes, which are whatever the access point beaconed and so may
    /// not be UTF-8. Empty for a hidden network, in either of the two ways an
    /// access point has of being one — see [`visible_ssid`].
    #[must_use]
    pub fn ssid(&self) -> &[u8] {
        &self.ssid[..usize::from(self.ssid_len)]
    }

    /// The roaming consortium element's body, verbatim as the beacon carried
    /// it. Empty when the access point beaconed none — most do not — or when
    /// what it beaconed was longer than the wire can carry, in which case it is
    /// dropped whole rather than truncated: a partly-arrived identifier is
    /// nobody's. What the bytes mean is [`rcoi_text`]'s to say.
    #[must_use]
    pub fn rcoi(&self) -> &[u8] {
        &self.rcoi[..usize::from(self.rcoi_len)]
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
            ext: self.rcoi(),
        }
    }
}

/// The name an access point actually beaconed, with a cloaked one's zero
/// padding stripped, and empty if there was nothing but padding.
///
/// Trailing zeros are padding and nothing else. An *interior* NUL is left alone: it
/// is not padding, and nothing here says what it was meant to be.
#[must_use]
pub fn visible_ssid(bytes: &[u8]) -> &[u8] {
    match bytes.iter().rposition(|&b| b != 0) {
        Some(last) => &bytes[..=last],
        None => &[],
    }
}

/// The roaming consortium element's body as WiGLE's `RCOIs` column spells it:
/// each identifier in hex — six digits for the three-byte form, ten for the
/// five-byte one — separated by single spaces.
///
/// The body is one count byte, one byte holding the first two identifiers'
/// lengths in its nibbles, then the identifiers themselves, the third (if any)
/// being whatever is left. A body whose claimed lengths do not fit is rendered
/// as nothing rather than in part: an identifier half-arrived is nobody's.
#[must_use]
pub fn rcoi_text(body: &[u8]) -> RcoiText<'_> {
    RcoiText(body)
}

/// The rendering [`rcoi_text`] returns. A `Display` rather than a function
/// returning a `String`, because this crate is `no_std` and allocation-free —
/// the same dodge `Security`'s WiGLE token uses — and the host asks for the
/// string where it has one to ask with.
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

/// One identifier: six hex digits for the three-byte form and ten for the
/// five-byte one, which are the two forms Hotspot 2.0 actually uses. Any other
/// length is spelled out plainly rather than padded into a shape it is not.
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
/// Split out because promiscuous mode delivers every frame on the channel and most
/// are data, so a caller rejects them before doing anything that costs — taking a
/// lock, most of all. Type 0 is management, and only these two subtypes carry the
/// elements.
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
/// Returns `None` for anything that is not a beacon (subtype 8) or probe response
/// (subtype 5), and for a frame too short to hold the fixed fields. Past that it is
/// best-effort: a malformed element ends the walk rather than discarding an access
/// point whose frame was received well enough to have a BSSID.
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
                // than dropping the access point. Clamp first and trim second —
                // the other order turns an over-long element whose only non-zero
                // byte is past the 32nd into a name made of padding, where
                // clamping first makes it the hidden network it is.
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
                // Roaming Consortium: what a Passpoint access point says about
                // which hotspot operators will authenticate you. Kept verbatim —
                // the count byte and the nibble-packed lengths byte included —
                // because the walk's job is to find it, not to read it;
                // [`rcoi_text`] does that, on the host, where a fix costs a
                // re-export rather than a reflash.
                //
                // Dropped whole rather than truncated when it exceeds what the
                // wire can carry: the pieces of an identifier say nothing the
                // whole one does. First wins, like the channel elements.
                111 if (2..=EXT_MAX).contains(&len) && self.rcoi_len == 0 => {
                    self.rcoi[..len].copy_from_slice(data);
                    // Cast is safe: `len` is at most EXT_MAX, which is 17.
                    #[allow(clippy::cast_possible_truncation)]
                    {
                        self.rcoi_len = len as u8;
                    }
                }
                // HT Operation, whose first byte is the primary channel: how a
                // 5 GHz access point says where it is, DS Parameter Set being a
                // 2.4 GHz element that 5 GHz beacons omit.
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

    /// Skip the version, group cipher and pairwise list, then read the AKM
    /// suites. The RSN and WPA bodies are the same shape once the OUI and type are
    /// stripped.
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
    /// Order matters, including the two rungs that fall
    /// through to [`Security::Undefined`]: the auth-mode table has no token for
    /// `WIFI_AUTH_OWE` or for WPA3-Enterprise, so both reach the WiGLE column as
    /// `[UNDEFINED]`. The column is a contract with an exporter rather than a
    /// description of the network.
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
        // `!has_rsn` on the second arm is deliberate, not a simplification: an
        // access point advertising 802.1X in its legacy WPA element and PSK in its
        // RSN element will negotiate the RSN one.
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
