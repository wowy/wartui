//! A phone's command text, read into a [`Command`], and the channel lists `-ch` and `ch` carry.
//!
//! The first word picks the command, matched exactly. Words the bridge has no mode for are
//! [`Command::Unsupported`], so the app hears a refusal rather than a help list. Words an app
//! sends to describe itself or keep the link are [`Command::Silent`]: they ask for no reply.
//!
//! **`wdstream` reads its arguments as the WardriveESP firmware does.** `stop`, `-ch `, `-bt`,
//! `-wifi`, `-cap` and `-promisc` match anywhere after the word, and the word `start` is not
//! required. `stop` wins over the rest, and `-bt` over `-wifi`. The app is closed, so the bridge
//! takes what that firmware takes.
//!
//! **`bt` switches the mode only when the rest is exactly `on` or `off`**; anything else shows it.
//!
//! **`reboot` alone is unknown on purpose.** The service takes commands from any phone in range,
//! with no bond and no encryption, so a command that restarts the bridge is one any passer-by
//! could send. `drop` and `link` are safe to take: a phone can drop only its own link, and `link`
//! tells it only about that link.
//!
//! **`bye` is its own command**: the app sends it before it lets go on purpose, and the advert
//! extras tell other phones so.
//!
//! **`iam` names the app**: `iam a <nickname>` for Android, `iam i <nickname>` for an iPhone. The
//! nickname is cut to [`NICKNAME_MAX`] bytes, then back to a whole UTF-8 character. Any other
//! form is [`Command::Silent`].
//!
//! A command is cut to [`COMMAND_MAX`] bytes before it is read, so the reply naming an unknown
//! word always fits a line.

use super::session::Mode;
use crate::plan::{ChannelPool, ChannelSet, NUM_SCAN_CHANNELS, SCAN_CHANNELS, index_of};

/// The most bytes of a command that are read.
pub const COMMAND_MAX: usize = 240;

/// The most bytes of an `iam` nickname that are kept.
pub const NICKNAME_MAX: usize = 18;

/// `text` cut to [`COMMAND_MAX`] bytes, without leading spaces or trailing spaces, CR and LF.
///
/// An inner `\n` is kept: one write is one command.
#[must_use]
pub fn trim(text: &[u8]) -> &[u8] {
    let text = &text[..text.len().min(COMMAND_MAX)];
    let end = text.iter().rposition(|b| !matches!(b, b' ' | b'\r' | b'\n')).map_or(0, |i| i + 1);
    let text = &text[..end];
    let start = text.iter().position(|&b| b != b' ').unwrap_or(text.len());
    &text[start..]
}

/// What `ch` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channels<'a> {
    /// `ch`: show the channels.
    Show,
    /// `ch all`: every channel.
    All,
    /// `ch 1,6,11`: these channels, as text.
    Set(&'a [u8]),
}

/// What `bt` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bt {
    /// `bt` with anything but `on` or `off`: show the mode.
    Show,
    /// `bt on`: scan Bluetooth.
    On,
    /// `bt off`: scan Wi-Fi.
    Off,
}

/// One command, as its text reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command<'a> {
    /// `wdstream start …`: `channels` is the text after `-ch `, to the end of the command.
    Stream {
        /// The channel list's text: after `-ch `, to the end of the command.
        channels: Option<&'a [u8]>,
        /// The command asks for a mode the bridge lacks: capture or promiscuous.
        unsupported_mode: bool,
        /// The mode asked: Bluetooth for `-bt`, else Wi-Fi for `-wifi`, else none.
        mode: Option<Mode>,
    },
    /// `stop`, or `wdstream` with `stop` in it.
    Stop,
    /// `start`.
    Start,
    /// `caps`.
    Caps,
    /// `help`.
    Help,
    /// `info`.
    Info,
    /// `status`.
    Status,
    /// `link`.
    Link,
    /// `drop`.
    Drop,
    /// `iam a <nickname>` or `iam i <nickname>`.
    Iam {
        /// The app runs on an iPhone, else on Android.
        iphone: bool,
        /// The nickname, cut to at most [`NICKNAME_MAX`] bytes on a UTF-8 character boundary.
        nickname: &'a [u8],
    },
    /// `ch`, in its three forms.
    Channels(Channels<'a>),
    /// `bt`, in its three forms.
    Bt(Bt),
    /// `bye`: the app is about to let go on purpose.
    Bye,
    /// A command the bridge takes and answers with nothing.
    Silent,
    /// A command for a mode the bridge lacks.
    Unsupported,
    /// Any other word.
    Unknown(&'a [u8]),
}

/// Whether `needle` appears in `hay`.
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// The text after the first `-ch `, to the end of `rest`, or `None` when no `-ch ` appears.
///
/// [`parse_channels`] stops at the first byte past the list, so the flags after it are left
/// there. A `-ch` that ends the command, or a longer flag such as `-chx`, names no list.
fn ch_argument(rest: &[u8]) -> Option<&[u8]> {
    let flag = b"-ch ";
    let at = rest.windows(flag.len()).position(|w| w == flag)?;
    Some(&rest[at + flag.len()..])
}

/// The `iam` that `rest` holds, or [`Command::Silent`] when it names no platform.
fn iam(rest: &[u8]) -> Command<'_> {
    let iphone = match rest {
        [b'a', b' ', ..] => false,
        [b'i', b' ', ..] => true,
        _ => return Command::Silent,
    };
    let nickname = &rest[2..];
    let mut end = nickname.len().min(NICKNAME_MAX);
    // Back to a whole character: a continuation byte at the cut belongs to the one before it.
    while end > 0 && end < nickname.len() && nickname[end] & 0b1100_0000 == 0b1000_0000 {
        end -= 1;
    }
    Command::Iam { iphone, nickname: &nickname[..end] }
}

/// The command `text` holds, or `None` when it is blank once trimmed.
#[must_use]
pub fn parse(text: &[u8]) -> Option<Command<'_>> {
    let text = trim(text);
    if text.is_empty() {
        return None;
    }
    let (word, rest) = match text.iter().position(|&b| b == b' ') {
        Some(at) => (&text[..at], &text[at + 1..]),
        None => (text, &[][..]),
    };
    Some(match word {
        b"wdstream" if contains(rest, b"stop") => Command::Stop,
        b"wdstream" => Command::Stream {
            channels: ch_argument(rest),
            unsupported_mode: contains(rest, b"-cap") || contains(rest, b"-promisc"),
            mode: if contains(rest, b"-bt") {
                Some(Mode::Bluetooth)
            } else if contains(rest, b"-wifi") {
                Some(Mode::Wifi)
            } else {
                None
            },
        },
        b"stop" => Command::Stop,
        b"start" => Command::Start,
        b"caps" => Command::Caps,
        b"help" => Command::Help,
        b"info" => Command::Info,
        b"status" => Command::Status,
        b"link" => Command::Link,
        b"drop" => Command::Drop,
        b"iam" => iam(rest),
        b"ch" => {
            let start = rest.iter().position(|&b| b != b' ').unwrap_or(rest.len());
            Command::Channels(match &rest[start..] {
                b"" => Channels::Show,
                b"all" => Channels::All,
                argument => Channels::Set(argument),
            })
        }
        b"bt" => Command::Bt(match rest {
            b"on" => Bt::On,
            b"off" => Bt::Off,
            _ => Bt::Show,
        }),
        b"bye" => Command::Bye,
        b"hb" | b"client" | b"time" => Command::Silent,
        b"cap" | b"csi" | b"coproc" | b"led" | b"screen" | b"sd" | b"ota" => Command::Unsupported,
        word => Command::Unknown(word),
    })
}

/// Channels a phone asked for, in the order it gave them, each once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelList {
    /// Channel numbers, in order.
    order: heapless::Vec<u8, { NUM_SCAN_CHANNELS as usize }>,
    /// The same channels, as [`SCAN_CHANNELS`] indices.
    set: ChannelSet,
}

impl ChannelList {
    const fn empty() -> Self {
        Self { order: heapless::Vec::new(), set: ChannelSet::empty() }
    }

    /// Every channel of `pool`, in [`SCAN_CHANNELS`] order.
    #[must_use]
    pub fn all(pool: ChannelPool) -> Self {
        let mut list = Self::empty();
        for channel in SCAN_CHANNELS {
            list.keep(channel, pool);
        }
        list
    }

    /// Channel numbers, in order.
    #[must_use]
    pub fn channels(&self) -> &[u8] {
        &self.order
    }

    /// Whether `channel` is on the list.
    #[must_use]
    pub fn contains(&self, channel: u8) -> bool {
        index_of(channel).is_some_and(|idx| self.set.contains(idx))
    }

    /// Add `channel` when `pool` holds it and the list does not yet.
    fn keep(&mut self, channel: u8, pool: ChannelPool) {
        let Some(idx) = index_of(channel) else { return };
        if !pool.contains(idx) || self.set.contains(idx) {
            return;
        }
        self.set.insert(idx);
        // A distinct table index each, so the table's length is room enough.
        let _ = self.order.push(channel);
    }
}

/// The channels `text` lists that `pool` holds, or `None` when it holds none of them.
///
/// Reads decimal numbers separated by commas or spaces, and stops at the first byte that is none
/// of these.
#[must_use]
pub fn parse_channels(text: &[u8], pool: ChannelPool) -> Option<ChannelList> {
    let mut list = ChannelList::empty();
    let mut number: Option<u16> = None;
    let mut finish = |number: &mut Option<u16>| {
        if let Some(channel) = number.take().and_then(|n| u8::try_from(n).ok()) {
            list.keep(channel, pool);
        }
    };
    for &byte in text {
        match byte {
            b'0'..=b'9' => {
                let digit = u16::from(byte - b'0');
                number = Some(number.unwrap_or(0).saturating_mul(10).saturating_add(digit));
            }
            b',' | b' ' => finish(&mut number),
            _ => break,
        }
    }
    finish(&mut number);
    (!list.order.is_empty()).then_some(list)
}
