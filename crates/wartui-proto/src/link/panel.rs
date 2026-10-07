use heapless::Vec;
use serde::Serialize;

use super::ShortStr;

/// The most rows a [`Panel`] may have, and so the most lines one
/// [`HostToBridge::ShowPanel`] carries.
///
/// A ceiling rather than a count: what a bridge actually has depends on the font it
/// draws in, it says so in [`BridgeToHost::Ready`], and a panel that reports fewer is
/// sent fewer. Eight leaves room above any font that fits five lines on the 80-pixel
/// screen this was written for, and the frame is sized against it.
///
/// [`HostToBridge::ShowPanel`]: super::HostToBridge::ShowPanel
/// [`BridgeToHost::Ready`]: super::BridgeToHost::Ready
pub const PANEL_ROWS: usize = 8;

/// A panel the bridge can draw lines of text on.
///
/// Announced in [`BridgeToHost::Ready`] rather than configured, so the host formats to the
/// geometry that is actually there and sends nothing at all to a bridge without a screen.
/// That is also why there is no operator flag for any of this.
///
/// [`BridgeToHost::Ready`]: super::BridgeToHost::Ready
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Panel {
    /// Characters that fit across one line.
    pub cols: u8,
    /// Lines that fit down the screen, never more than [`PANEL_ROWS`].
    pub rows: u8,
}

/// How a line is going.
///
/// The bridge maps this to a colour and the host decides which one a line has, because the
/// thresholds are the host's to know: what counts as a weak link is arithmetic over a
/// snapshot, and retuning it must not cost a reflash. The colours themselves are a property
/// of the panel and live in the firmware.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum Severity {
    /// Fine.
    Ok,
    /// Working, but not as it should be.
    Warn,
    /// A fault, and the capture is the worse for it.
    Error,
}

/// One row of the panel, laid out by the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct PanelLine {
    /// How the thing this line reports is going.
    pub level: Severity,
    /// The text, already truncated to the panel's width.
    pub text: ShortStr,
}

/// A whole panel's worth of lines.
pub type PanelLines = Vec<PanelLine, PANEL_ROWS>;
