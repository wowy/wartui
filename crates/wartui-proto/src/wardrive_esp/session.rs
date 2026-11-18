//! A phone's WardriveESP session: what the P4 keeps between commands, and the lines each event
//! sends.
//!
//! Pure: the P4 passes in the time and its own state ([`Ctx`]), and sends what [`Sink`] is given.
//! An [`Ask`] returned asks the P4 to start or stop the fleet's Wi-Fi scan for the phone, or to
//! drop the phone's link.
//!
//! **Lines come in two classes** ([`Class`]), because the P4 queues them differently.
//! [`Class::Stream`] lines are the scan's own, `WD:AP` among them, and keep their order with it:
//! an app reads `WD:END` as the last line of a scan. [`Class::Answer`] lines reply to a command
//! and may overtake a backlog of stream lines, so a busy scan still answers. `WD:HEALTH` is an
//! answer too, so a backlog never holds it.
//!
//! The channel list, the app's `iam` and its `bye` are the session's: a list of `None` means every
//! channel of the pool. They last until the phone subscribes again or lets go
//! ([`Session::release`]), so a stop and a start keep them. The P4 reads [`Session::said_bye`]
//! before the release, for how the link ended, and [`Session::beacon`] for what the status beacon
//! says. The free-text replies are wartui's own wording.

use heapless::Vec;

use super::beacon::{Beacon, LinkEnd, channel_bits};
use super::command::{self, ChannelList, Channels, Command, NICKNAME_MAX, parse_channels};
use super::line::{self, Commas, FW, Line};
use crate::bus::{RecordKind, SightingMsg};
use crate::plan::ChannelPool;
use crate::radio_link::PhoneLink;
use crate::reset::ResetCause;

/// How the P4 queues a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// A reply to a command, or `WD:HEALTH`: may overtake stream lines.
    Answer,
    /// A line of the scan: keeps its order with the `WD:AP` lines.
    Stream,
}

/// Where a session's lines go.
pub trait Sink {
    /// Send one whole line, its `\n` included.
    fn line(&mut self, class: Class, line: &[u8]);
}

/// What a session asks of the bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    /// Start scanning Wi-Fi.
    Start,
    /// Stop scanning.
    Stop,
    /// Drop the phone's link.
    Drop,
}

/// The P4's booted image, as `WD:BOOT` reads it.
#[derive(Debug, Clone, Copy)]
pub struct Boot<'a> {
    /// The running slot's label, or `?`.
    pub slot: &'a str,
    /// The image is on trial.
    pub trial: bool,
    /// Why the P4 last reset.
    pub reset: ResetCause,
}

/// The bridge's state, as a command's answer reads it.
#[derive(Debug, Clone, Copy)]
pub struct Ctx<'a> {
    /// The channel pool.
    pub pool: ChannelPool,
    /// The bridge firmware's own version.
    pub version: &'a str,
    /// The bridge's last two address octets.
    pub bridge: [u8; 2],
    /// Milliseconds since the bridge booted.
    pub uptime_ms: u32,
    /// Fleet members.
    pub nodes: u8,
    /// The phone link's ATT MTU.
    pub mtu: u16,
    /// Lines dropped from the phone's queue.
    pub dropped: u32,
    /// The phone's Bluetooth link, as the C6 last reported it.
    pub link: PhoneLink,
    /// The P4's booted image.
    pub boot: Boot<'a>,
}

/// What the app said it is with `iam`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Iam {
    /// The app runs on an iPhone, else on Android.
    iphone: bool,
    /// The nickname's bytes, as they came.
    nickname: Vec<u8, NICKNAME_MAX>,
}

/// A signal reading: `-61 dBm`, or `unknown` before the first.
struct Signal(Option<i8>);

impl core::fmt::Display for Signal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Some(rssi) => write!(f, "{rssi} dBm"),
            None => f.write_str("unknown"),
        }
    }
}

/// One phone's session. See the module documentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    scanning: bool,
    /// The channels the phone asked for, or `None` for every channel.
    list: Option<ChannelList>,
    /// `WD:AP` lines since the last period.
    aps: u32,
    /// Periods since the scan started.
    scans: u32,
    /// `aps` at the last period.
    last_aps: u32,
    /// The app's latest `iam`, or `None` before one.
    iam: Option<Iam>,
    /// The app sent `bye`.
    bye: bool,
}

/// Build a line with `build` and send it, or send nothing when it does not fit.
fn emit(sink: &mut impl Sink, class: Class, build: impl FnOnce(&mut Line) -> bool) {
    let mut out = Line::new();
    if build(&mut out) {
        sink.line(class, &out);
    }
}

/// Send `lines` as free text, each an answer.
fn answer(sink: &mut impl Sink, lines: &[core::fmt::Arguments<'_>]) {
    for args in lines {
        emit(sink, Class::Answer, |out| line::text(out, *args));
    }
}

/// The help list.
fn help(sink: &mut impl Sink) {
    const LINES: [&str; 14] = [
        "wartui bridge, WardriveESP commands:",
        "  wdstream start -wifi [-ch 1,6,11]",
        "  wdstream stop",
        "  start",
        "  stop",
        "  caps",
        "  ch",
        "  ch 1,6,11",
        "  ch all",
        "  status",
        "  info",
        "  link",
        "  drop",
        "  help",
    ];
    for text in LINES {
        emit(sink, Class::Answer, |out| line::text(out, format_args!("{text}")));
    }
}

impl Session {
    /// No phone, not scanning, every channel.
    #[must_use]
    pub const fn new() -> Self {
        Self { scanning: false, list: None, aps: 0, scans: 0, last_aps: 0, iam: None, bye: false }
    }

    /// Whether the session is scanning.
    #[must_use]
    pub const fn scanning(&self) -> bool {
        self.scanning
    }

    /// Whether the app sent `bye`.
    #[must_use]
    pub const fn said_bye(&self) -> bool {
        self.bye
    }

    /// What the status beacon says of this session, over `pool`, with the last link's end as
    /// `link_end`.
    #[must_use]
    pub fn beacon(&self, pool: ChannelPool, link_end: LinkEnd) -> Beacon {
        let (iphone, nickname) =
            self.iam.as_ref().map_or((false, Vec::new()), |iam| (iam.iphone, iam.nickname.clone()));
        Beacon {
            scanning: self.scanning,
            count: u16::try_from(self.last_aps).unwrap_or(u16::MAX),
            iphone,
            nickname,
            link_end,
            channels: channel_bits(self.list(pool).channels()),
        }
    }

    /// The phone subscribed to TX: a fresh session, scanning.
    pub fn on_subscribe(&mut self, ctx: &Ctx<'_>, sink: &mut impl Sink) -> Option<Ask> {
        *self = Self::new();
        self.start(ctx, sink)
    }

    /// The phone sent `text`.
    pub fn on_command(&mut self, text: &[u8], ctx: &Ctx<'_>, sink: &mut impl Sink) -> Option<Ask> {
        let command = command::parse(text)?;
        match command {
            Command::Silent => None,
            Command::Bye => {
                self.bye = true;
                None
            }
            Command::Stream { unsupported_mode: true, .. } | Command::Unsupported => {
                emit(sink, Class::Answer, |out| line::err(out, "unsupported"));
                None
            }
            Command::Stream { channels, .. } => {
                if let Some(text) = channels {
                    self.set_channels(text, ctx, sink);
                }
                if self.scanning {
                    emit(sink, Class::Answer, line::mode);
                    None
                } else {
                    self.start(ctx, sink)
                }
            }
            Command::Start => {
                if self.scanning {
                    None
                } else {
                    self.start(ctx, sink)
                }
            }
            Command::Stop => self.stop(sink),
            Command::Caps => {
                emit(sink, Class::Answer, |out| line::caps(out, ctx.pool));
                None
            }
            Command::Channels(Channels::Show) => {
                let pool = ChannelList::all(ctx.pool);
                let list = self.list(ctx.pool);
                answer(
                    sink,
                    &[
                        format_args!("supported {}", Commas(pool.channels())),
                        format_args!("scanning {}", Commas(list.channels())),
                    ],
                );
                None
            }
            Command::Channels(Channels::All) => {
                self.list = None;
                if self.scanning {
                    let list = self.list(ctx.pool);
                    emit(sink, Class::Answer, |out| line::plan(out, &list));
                }
                let count = ctx.pool.channel_count();
                answer(sink, &[format_args!("scanning all {count} channels")]);
                None
            }
            Command::Channels(Channels::Set(text)) => {
                if self.set_channels(text, ctx, sink) {
                    let list = self.list(ctx.pool);
                    answer(sink, &[format_args!("scanning {}", Commas(list.channels()))]);
                }
                None
            }
            Command::Status => {
                let list = self.list(ctx.pool);
                let state = if self.scanning { "on" } else { "off" };
                answer(
                    sink,
                    &[
                        format_args!("scanning {state}"),
                        format_args!("aps {} scans {}", self.last_aps, self.scans),
                        format_args!("channels {}", Commas(list.channels())),
                        format_args!("mtu {} dropped {}", ctx.mtu, ctx.dropped),
                    ],
                );
                None
            }
            Command::Info => {
                let [a, b] = ctx.bridge;
                answer(
                    sink,
                    &[
                        format_args!("wartui bridge {a:02X}:{b:02X} fw {}", ctx.version),
                        format_args!("WardriveESP protocol {FW}"),
                        format_args!("uptime {} s nodes {}", ctx.uptime_ms / 1000, ctx.nodes),
                    ],
                );
                emit(sink, Class::Answer, |out| line::boot(out, &ctx.boot));
                None
            }
            Command::Link => {
                self.link(ctx, sink);
                None
            }
            Command::Drop => {
                answer(sink, &[format_args!("disconnecting the phone")]);
                Some(Ask::Drop)
            }
            Command::Iam { iphone, nickname } => {
                let mut kept = Vec::new();
                // The command cut it to the room.
                let _ = kept.extend_from_slice(nickname);
                self.iam = Some(Iam { iphone, nickname: kept });
                None
            }
            Command::Help => {
                help(sink);
                None
            }
            Command::Unknown(word) => {
                emit(sink, Class::Answer, |out| line::unknown(out, word));
                help(sink);
                None
            }
        }
    }

    /// A node heard `sighting`, `ts_ms` after the bridge booted.
    ///
    /// Sent only while scanning, only for an access point, and only on a listed channel.
    pub fn on_sighting(&mut self, ts_ms: u32, sighting: &SightingMsg<'_>, sink: &mut impl Sink) {
        if !self.scanning || sighting.kind != RecordKind::Wifi {
            return;
        }
        if self.list.as_ref().is_some_and(|list| !list.contains(sighting.channel)) {
            return;
        }
        emit(sink, Class::Stream, |out| line::ap(out, ts_ms, sighting));
        self.aps = self.aps.saturating_add(1);
    }

    /// A scan period ended: `WD:STATUS` with the access points sent in it.
    pub fn on_period(&mut self, sink: &mut impl Sink) {
        if !self.scanning {
            return;
        }
        self.scans = self.scans.saturating_add(1);
        let (aps, scans) = (self.aps, self.scans);
        emit(sink, Class::Stream, |out| line::status(out, aps, scans));
        self.last_aps = aps;
        self.aps = 0;
    }

    /// `WD:HEALTH`, an answer, scanning or not.
    pub fn on_health(&self, ctx: &Ctx<'_>, sink: &mut impl Sink) {
        let up_s = ctx.uptime_ms / 1000;
        emit(sink, Class::Answer, |out| line::health(out, up_s, &ctx.link, ctx.mtu, ctx.dropped));
    }

    /// The phone let go: a fresh session, and a stop if it was scanning.
    pub fn release(&mut self) -> Option<Ask> {
        let ask = self.scanning.then_some(Ask::Stop);
        *self = Self::new();
        ask
    }

    /// The session's list, or the whole pool.
    fn list(&self, pool: ChannelPool) -> ChannelList {
        self.list.clone().unwrap_or_else(|| ChannelList::all(pool))
    }

    /// Answer `link`: how long the phone has held the link, who it said it is, the link's
    /// figures, and how the link before it ended.
    fn link(&self, ctx: &Ctx<'_>, sink: &mut impl Sink) {
        let link = &ctx.link;
        let [a, b] = link.peer;
        let s = link.connected_s;
        let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
        answer(
            sink,
            &[format_args!("Bluetooth: connected to {a:02X}:{b:02X} for {h}h {m:02}m {s:02}s")],
        );
        match &self.iam {
            Some(iam) => {
                let platform = if iam.iphone { "iPhone" } else { "Android" };
                emit(sink, Class::Answer, |out| line::held_by(out, &iam.nickname, platform));
            }
            None => answer(sink, &[format_args!("  held by an app that gave no name")]),
        }
        let (signal, mtu) = (Signal(link.rssi), ctx.mtu);
        let (interval, timeout) = (link.interval_us / 1000, link.timeout_ms);
        answer(
            sink,
            &[format_args!(
                "  signal {signal}, MTU {mtu}, interval {interval} ms, timeout {timeout} ms"
            )],
        );
        let links = link.connections;
        match link.last_drop {
            Some(drop) => answer(
                sink,
                &[format_args!(
                    "  links since boot {links}, last drop {} s ago (0x{:02X})",
                    drop.ago_s, drop.reason
                )],
            ),
            None => answer(sink, &[format_args!("  links since boot {links}, no drops")]),
        }
    }

    /// Start scanning from zero counts and send the header.
    fn start(&mut self, ctx: &Ctx<'_>, sink: &mut impl Sink) -> Option<Ask> {
        self.scanning = true;
        self.aps = 0;
        self.scans = 0;
        self.last_aps = 0;
        let list = self.list(ctx.pool);
        emit(sink, Class::Stream, |out| line::caps(out, ctx.pool));
        emit(sink, Class::Stream, |out| line::boot(out, &ctx.boot));
        emit(sink, Class::Stream, |out| line::link(out, &ctx.link, ctx.mtu));
        emit(sink, Class::Stream, line::mode);
        emit(sink, Class::Stream, line::begin);
        emit(sink, Class::Stream, |out| line::plan(out, &list));
        Some(Ask::Start)
    }

    /// Stop scanning: the last `WD:STATUS`, then `WD:END`. Nothing while idle.
    fn stop(&mut self, sink: &mut impl Sink) -> Option<Ask> {
        if !self.scanning {
            return None;
        }
        self.scanning = false;
        let (aps, scans) = (self.aps, self.scans);
        emit(sink, Class::Stream, |out| line::status(out, aps, scans));
        emit(sink, Class::Stream, line::end);
        Some(Ask::Stop)
    }

    /// Replace the list with the one `text` names, saying whether it named one.
    ///
    /// A new list is announced with `WD:PLAN` while scanning. A list naming no channel of the pool
    /// is refused, and the old one stays.
    fn set_channels(&mut self, text: &[u8], ctx: &Ctx<'_>, sink: &mut impl Sink) -> bool {
        let Some(list) = parse_channels(text, ctx.pool) else {
            emit(sink, Class::Answer, |out| line::err(out, "no_supported_channels"));
            return false;
        };
        if self.scanning {
            emit(sink, Class::Answer, |out| line::plan(out, &list));
        }
        self.list = Some(list);
        true
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}
