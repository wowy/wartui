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
//!
//! **`WD:BOOT`, `WD:LINK` and `WD:HEALTH` carry only the keys the bridge has a figure for.**
//! The document's `boots`, `prev_fw`, `prev_stage` and `rolled_back` are left out of `WD:BOOT`,
//! and its `largest`, `stack_*`, `low_stack`, `last`, `batt` and `temp` out of `WD:HEALTH`: the
//! bridge keeps none of them. `heap` and `heap_min` are the C6's, since the P4 has no heap.
//! `prev_disc` is `0x200` plus the HCI reason, the form the WardriveESP firmware's Bluetooth
//! stack reports and apps read.

use core::fmt::{self, Write as _};

use super::command::ChannelList;
use super::session::Boot;
use crate::bus::{Security, SightingMsg};
use crate::plan::ChannelPool;
use crate::radio_link::PhoneLink;
use crate::reset::ResetCause;

/// The longest line, its `\n` included.
pub const LINE_MAX: usize = 320;

/// One output line.
pub type Line = heapless::Vec<u8, LINE_MAX>;

/// The WardriveESP protocol level `WD:CAPS` names: [`FW_LEVEL`], spelled out.
pub const FW: &str = "1.6.0";

/// The protocol level as major, minor and patch, which the status beacon carries.
pub const FW_LEVEL: (u8, u8, u8) = (1, 6, 0);

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

/// `WD:BOOT`: the P4's booted image and why it last reset.
pub fn boot(out: &mut Line, boot: &Boot<'_>) -> bool {
    let image = if boot.trial { "trial" } else { "confirmed" };
    build(out, |out| {
        write!(
            out,
            "WD:BOOT fw={FW} slot={} image={image} reset={}",
            boot.slot,
            reset_key(boot.reset)
        )
    })
}

/// `WD:LINK`: the connections since the C6 booted, the MTU, and how the one before this ended.
pub fn link(out: &mut Line, link: &PhoneLink, mtu: u16) -> bool {
    build(out, |out| {
        write!(out, "WD:LINK conns={} mtu={mtu} ", link.connections)?;
        match link.last_drop {
            Some(drop) => write!(
                out,
                "prev_disc=0x{:03x} prev_disc_ago={}s",
                0x200 + u16::from(drop.reason),
                drop.ago_s
            ),
            None => out.write_str("prev_disc=none"),
        }
    })
}

/// `WD:HEALTH`: `up_s` since the P4 booted, the C6's heap, the MTU, and the lines `dropped`.
pub fn health(out: &mut Line, up_s: u32, link: &PhoneLink, mtu: u16, dropped: u32) -> bool {
    build(out, |out| {
        write!(
            out,
            "WD:HEALTH up={up_s} heap={} heap_min={} mtu={mtu} txdrop={dropped}",
            link.heap_free, link.heap_least
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

/// `  held by <nickname> (<platform>)`, with `nickname`'s bytes as they came.
pub(super) fn held_by(out: &mut Line, nickname: &[u8], platform: &str) -> bool {
    build(out, |out| {
        out.extend_from_slice(b"  held by ")
            .and_then(|()| out.extend_from_slice(nickname))
            .map_err(|_| fmt::Error)?;
        write!(out, " ({platform})")
    })
}

/// The `reset=` key for `cause`.
#[must_use]
pub const fn reset_key(cause: ResetCause) -> &'static str {
    match cause {
        ResetCause::PowerOn => "power_on",
        ResetCause::Software => "software",
        ResetCause::Watchdog | ResetCause::Lockup => "watchdog",
        ResetCause::Brownout => "brownout",
        ResetCause::External => "reset_pin",
        ResetCause::Unknown => "other",
    }
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
