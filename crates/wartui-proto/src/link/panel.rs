use heapless::Vec;
use serde::Serialize;

use super::ShortStr;

/// The most rows a [`Panel`] may have, and so the most lines one
/// [`HostToBridge::ShowPanel`] carries.
///
/// A ceiling, not a count. A bridge's real row count depends on its font, and it reports
/// that in [`BridgeToHost::Ready`]. Eight leaves room above any font that fits five lines
/// on an 80-pixel screen, and the link frame is sized against it.
///
/// [`HostToBridge::ShowPanel`]: super::HostToBridge::ShowPanel
/// [`BridgeToHost::Ready`]: super::BridgeToHost::Ready
pub const PANEL_ROWS: usize = 8;

/// A panel the bridge can draw lines of text on.
///
/// Announced in [`BridgeToHost::Ready`] rather than configured. The host formats to the
/// geometry that is there, and sends nothing to a bridge without a screen. That is why
/// there is no operator flag for it.
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
/// The host picks a line's severity and the bridge maps it to a color. What counts as a
/// weak link is arithmetic over a snapshot, and retuning it must not cost a reflash. The
/// colors are a property of the panel, and live in the firmware.
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
