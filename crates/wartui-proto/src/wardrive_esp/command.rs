//! A phone's command text, read into a [`Command`], and the channel lists `-ch` and `ch` carry.
//!
//! The first word picks the command, matched exactly. Words the bridge has no mode for are
//! [`Command::Unsupported`], so the app hears a refusal rather than a help list. Words an app
//! sends to describe itself or keep the link are [`Command::Silent`]: they ask for no reply.
//!
//! **`reboot`, `drop` and `link` are unknown on purpose.** The service takes commands from any
//! phone in range, with no bond and no encryption, so a command that restarts the bridge or drops
//! its link is one any passer-by could send.
//!
//! A command is cut to [`COMMAND_MAX`] bytes before it is read, so the reply naming an unknown
//! word always fits a line.

use crate::plan::{ChannelPool, ChannelSet, NUM_SCAN_CHANNELS, SCAN_CHANNELS, index_of};

/// The most bytes of a command that are read.
pub const COMMAND_MAX: usize = 240;

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

/// One command, as its text reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command<'a> {
    /// `wdstream start …`: `channels` is the text after `-ch `, when given.
    Stream {
        /// The channel list's text.
        channels: Option<&'a [u8]>,
        /// The command asks for a mode the bridge lacks: Bluetooth, capture or promiscuous.
        unsupported_mode: bool,
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
    /// `ch`, in its three forms.
    Channels(Channels<'a>),
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

/// The text after the first `-ch` followed by a space or the end, up to the next space.
fn ch_argument(rest: &[u8]) -> Option<&[u8]> {
    let flag = b"-ch";
    let at = (0..=rest.len().saturating_sub(flag.len())).find(|&i| {
        rest[i..].starts_with(flag) && rest.get(i + flag.len()).is_none_or(|&b| b == b' ')
    })?;
    let after = rest.get(at + flag.len() + 1..).unwrap_or_default();
    let end = after.iter().position(|&b| b == b' ').unwrap_or(after.len());
    Some(&after[..end])
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
            unsupported_mode: [&b"-bt"[..], b"-cap", b"-promisc"]
                .iter()
                .any(|flag| contains(rest, flag)),
        },
        b"stop" => Command::Stop,
        b"start" => Command::Start,
        b"caps" => Command::Caps,
        b"help" => Command::Help,
        b"info" => Command::Info,
        b"status" => Command::Status,
        b"ch" => {
            let start = rest.iter().position(|&b| b != b' ').unwrap_or(rest.len());
            Command::Channels(match &rest[start..] {
                b"" => Channels::Show,
                b"all" => Channels::All,
                argument => Channels::Set(argument),
            })
        }
        b"bye" | b"iam" | b"hb" | b"client" | b"time" => Command::Silent,
        b"bt" | b"cap" | b"csi" | b"coproc" | b"led" | b"screen" | b"sd" | b"ota" => {
            Command::Unsupported
        }
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
/// Reads decimal numbers separated by commas, and stops at the first byte that is neither.
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
            b',' => finish(&mut number),
            _ => break,
        }
    }
    finish(&mut number);
    (!list.order.is_empty()).then_some(list)
}
