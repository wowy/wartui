//! The WardriveESP output lines the bridge sends, written into a [`Line`].
//!
//! Each function writes one whole line ending `\n`, or nothing: a line that does not fit
//! [`LINE_MAX`] is dropped whole rather than cut, because an app splits output on `\n` and would
//! read a cut line as a corrupt one.
//!
//! **`WD:CAPS` names `board=wartui`.** An app that does not know a `board=` treats the board as
//! partner hardware and offers it no firmware update, and `ota=0` says the same: the bridge is
//! updated by wartui, never by the phone. Every other capability is `0` but the two bands, since
//! the bridge offers only Wi-Fi.

use core::fmt::{self, Write as _};

use super::command::ChannelList;
use crate::bus::{Security, SightingMsg};
use crate::plan::ChannelPool;

/// The longest line, its `\n` included.
pub const LINE_MAX: usize = 320;

/// One output line.
pub type Line = heapless::Vec<u8, LINE_MAX>;

/// The WardriveESP protocol level `WD:CAPS` names.
pub const FW: &str = "1.6.0";

/// Channel numbers, comma-separated.
pub(super) struct Commas<'a>(pub &'a [u8]);

impl fmt::Display for Commas<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, channel) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_char(',')?;
            }
            write!(f, "{channel}")?;
        }
        Ok(())
    }
}

/// Clear `out` and write a line with `body`, saying whether it fit. `out` is empty when not.
fn build(out: &mut Line, body: impl FnOnce(&mut Line) -> fmt::Result) -> bool {
    out.clear();
    let fit = body(out).is_ok() && out.push(b'\n').is_ok();
    if !fit {
        out.clear();
    }
    fit
}

/// `WD:CAPS`: what the bridge offers, with `pool`'s channels.
pub fn caps(out: &mut Line, pool: ChannelPool) -> bool {
    let list = ChannelList::all(pool);
    build(out, |out| {
        write!(
            out,
            "WD:CAPS fw={FW} chip=esp32c5 bands=2g,5g ota=0 bt=0 gps=0 cap=0 usb=0 board=wartui \
             board_name=wartui link=ble chans={}",
            Commas(list.channels())
        )
    })
}

/// `WD:MODE`: the bridge scans Wi-Fi.
pub fn mode(out: &mut Line) -> bool {
    build(out, |out| out.write_str("WD:MODE mode=wifi"))
}

/// `WD:BEGIN`: a scan started.
pub fn begin(out: &mut Line) -> bool {
    build(out, |out| out.write_str("WD:BEGIN type=wifi interval=0 channel=auto"))
}

/// `WD:PLAN`: the channels `list` holds.
pub fn plan(out: &mut Line, list: &ChannelList) -> bool {
    build(out, |out| write!(out, "WD:PLAN chans={}", Commas(list.channels())))
}

/// `WD:AP`: one access point, heard `ts_ms` after the bridge booted.
pub fn ap(out: &mut Line, ts_ms: u32, sighting: &SightingMsg<'_>) -> bool {
    build(out, |out| {
        let [a, b, c, d, e, f] = sighting.bssid;
        write!(out, "WD:AP ts={ts_ms} bssid={a:02X}:{b:02X}:{c:02X}:{d:02X}:{e:02X}:{f:02X} ")?;
        out.write_str("ssid_hex=")?;
        for byte in sighting.ssid {
            write!(out, "{byte:02x}")?;
        }
        write!(
            out,
            " rssi={} ch={} auth={} hidden={}",
            sighting.rssi,
            sighting.channel,
            auth_token(sighting.security),
            u8::from(sighting.ssid.is_empty())
        )
    })
}

/// `WD:STATUS`: `aps` heard since the last one, after `scans` periods.
pub fn status(out: &mut Line, aps: u32, scans: u32) -> bool {
    build(out, |out| write!(out, "WD:STATUS aps={aps} scans={scans}"))
}

/// `WD:END`: the scan stopped.
pub fn end(out: &mut Line) -> bool {
    build(out, |out| out.write_str("WD:END reason=stop"))
}

/// `WD:ERR`: a command refused, for `reason`.
pub fn err(out: &mut Line, reason: &str) -> bool {
    build(out, |out| write!(out, "WD:ERR reason={reason}"))
}

/// A free-text reply line.
pub fn text(out: &mut Line, args: fmt::Arguments<'_>) -> bool {
    build(out, |out| out.write_fmt(args))
}

/// `unknown command '<word>'`, with `word`'s bytes as they came, UTF-8 or not.
pub(super) fn unknown(out: &mut Line, word: &[u8]) -> bool {
    build(out, |out| {
        out.extend_from_slice(b"unknown command '")
            .and_then(|()| out.extend_from_slice(word))
            .and_then(|()| out.extend_from_slice(b"'"))
            .map_err(|_| fmt::Error)
    })
}

/// The `auth=` token for `security`.
#[must_use]
pub const fn auth_token(security: Security) -> &'static str {
    match security {
        Security::Open => "OPEN",
        Security::Wep => "WEP",
        Security::WpaPsk => "WPA",
        Security::Wpa2Psk | Security::WpaWpa2Psk | Security::Wpa2Enterprise => "WPA2",
        Security::Wpa3Psk => "WPA3",
        Security::Wpa2Wpa3Psk => "WPA2_WPA3",
        Security::WapiPsk | Security::Undefined | Security::Ble | Security::Unknown(_) => "UNKNOWN",
    }
}
