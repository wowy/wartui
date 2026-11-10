//! The unique counts are drawn big at every screen size, because they are what a driver glances
//! at. The wide layout stacks them at the left of the strip; the narrow one puts them side by
//! side above the chart, and stacks them too when both do not fit beside each other. Only a strip
//! too small for one big glyph reduces them to a line of text.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tui_big_text::{BigText, PixelSize};
use wartui_core::engine::Snapshot;

use crate::text::thousands;

/// Rows one group takes: one row of Quadrant glyphs.
const GROUP_H: u16 = 4;

/// Columns a Quadrant glyph takes.
const GLYPH_W: u16 = 4;

/// Where the second group starts when the two sit side by side.
const SECOND_AT: u16 = 27;

/// The narrowest the wide layout's counts column is.
const WIDE_W: u16 = 29;

/// Below this width, or below [`GROUP_H`] rows, the counts are one line of text.
const MIN_W: u16 = 24;

/// A count rendered short for a narrow line. Three significant figures, because these are estimates
/// with about 0.8% standard error ([`wartui_core::distinct`]), so later digits are noise.
pub(super) fn approx(n: u64) -> String {
    if n < 10_000 {
        return n.to_string();
    }
    let (k, m) = (n as f64 / 1e3, n as f64 / 1e6);
    // Each unit is judged on the figure as it would be printed, not the raw count, so one
    // that rounds up past its unit's range (99,950 to "100.0k") moves on to the next.
    for (value, digits, below, unit) in [(k, 1, 100.0, "k"), (k, 0, 1000.0, "k"), (m, 2, 10.0, "M")]
    {
        let text = format!("{value:.digits$}");
        if text.parse::<f64>().is_ok_and(|shown| shown < below) {
            return format!("{text}{unit}");
        }
    }
    format!("{m:.1}M")
}

/// One kind's count: `~`, the estimate in big glyphs, and two labels beside them.
struct Group {
    digits: String,
    style: Style,
    labels: [String; 2],
}

impl Group {
    /// The Wi-Fi and Bluetooth groups.
    fn both(snapshot: &Snapshot) -> [Self; 2] {
        let c = &snapshot.counters;
        [
            Self {
                digits: approx(snapshot.unique_wifi_aps),
                style: Style::new().fg(Color::Green),
                labels: [
                    "unique APs".to_owned(),
                    format!("{} wifi", thousands(c.wifi_observations)),
                ],
            },
            Self {
                digits: approx(snapshot.unique_ble_aps),
                style: Style::new().fg(Color::Blue),
                labels: ["unique BLE".to_owned(), format!("{} bt", thousands(c.ble_observations))],
            },
        ]
    }

    /// Where the labels start, from the group's left.
    fn label_at(&self) -> u16 {
        let glyphs = u16::try_from(self.digits.chars().count()).unwrap_or(u16::MAX);
        glyphs.saturating_mul(GLYPH_W).saturating_add(4)
    }

    fn width(&self) -> u16 {
        let label = self.labels.iter().map(|l| l.chars().count()).max().unwrap_or(0);
        self.label_at().saturating_add(u16::try_from(label).unwrap_or(u16::MAX))
    }

    /// Draw the group with its top left at `x`, `y`, clipped to `clip`.
    fn draw(&self, frame: &mut Frame<'_>, x: u16, y: u16, clip: Rect) {
        let at = |dx: u16, dy: u16, width: u16, height: u16| {
            Rect::new(x.saturating_add(dx), y.saturating_add(dy), width, height).intersection(clip)
        };
        frame.render_widget(Paragraph::new("~"), at(0, 1, 1, 1));
        let big = BigText::builder()
            .pixel_size(PixelSize::Quadrant)
            .style(self.style.add_modifier(Modifier::BOLD))
            .lines(vec![Line::from(self.digits.clone())])
            .build();
        frame.render_widget(big, at(2, 0, self.label_at().saturating_sub(4), GROUP_H));
        for (label, dy) in self.labels.iter().zip([0, 2]) {
            let width = u16::try_from(label.chars().count()).unwrap_or(u16::MAX);
            frame.render_widget(Paragraph::new(label.clone()), at(self.label_at(), dy, width, 1));
        }
    }
}

/// The two groups, built once a frame for the layout and the drawing.
pub(super) struct Counts {
    groups: [Group; 2],
}

impl Counts {
    pub(super) fn new(snapshot: &Snapshot) -> Self {
        Self { groups: Group::both(snapshot) }
    }

    /// The width the wide layout's counts column takes: [`WIDE_W`], or more when a group needs it.
    pub(super) fn wide_width(&self) -> u16 {
        self.groups.iter().map(Group::width).fold(WIDE_W, u16::max)
    }

    /// Where the second group starts beside the first, clear of it.
    fn second_at(&self) -> u16 {
        SECOND_AT.max(self.groups[0].width().saturating_add(2))
    }

    /// Whether both groups fit side by side in `width`.
    fn side_by_side_fits(&self, width: u16) -> bool {
        width >= self.second_at().saturating_add(self.groups[1].width())
    }

    /// The rows the narrow layout's counts band takes at `width`: one group's height when both
    /// fit side by side, two when they stack.
    pub(super) fn band_height(&self, width: u16) -> u16 {
        if width < MIN_W {
            return 1;
        }
        if self.side_by_side_fits(width) { GROUP_H } else { 2 * GROUP_H }
    }

    /// Draw the counts into `area`: stacked when `stacked` and the rows allow, side by side when
    /// the columns allow, and as text when neither does.
    pub(super) fn draw(&self, frame: &mut Frame<'_>, area: Rect, stacked: bool) {
        let [wifi, ble] = &self.groups;
        let tall = area.height >= 2 * GROUP_H;
        let beside = area.height >= GROUP_H && self.side_by_side_fits(area.width);
        if area.height < GROUP_H || area.width < MIN_W || !(tall || beside) {
            frame.render_widget(Paragraph::new(self.text_line()), area);
        } else if beside && !(stacked && tall) {
            wifi.draw(frame, area.x, area.y, area);
            ble.draw(frame, area.x.saturating_add(self.second_at()), area.y, area);
        } else {
            wifi.draw(frame, area.x, area.y, area);
            ble.draw(frame, area.x, area.y.saturating_add(GROUP_H), area);
        }
    }

    /// The counts as one line, for a strip too small for big glyphs: ` ~95 APs  ~20 BLE`.
    fn text_line(&self) -> Line<'static> {
        let [wifi, ble] = &self.groups;
        let bold = |color| Style::new().fg(color).add_modifier(Modifier::BOLD);
        Line::from(vec![
            Span::styled(format!(" ~{} APs", wifi.digits), bold(Color::Green)),
            Span::styled(format!("  ~{} BLE", ble.digits), bold(Color::Blue)),
        ])
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::tui::draw;
    use crate::tui::fixtures::*;
    use crate::tui::ui::Ui;

    fn screen(snapshot: &Snapshot, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test backend");
        terminal.draw(|frame| draw(frame, snapshot, &mut Ui::default())).expect("drawing");
        terminal.backend().to_string()
    }

    /// `text` in big glyphs, as rows of text.
    fn big(text: &str) -> Vec<String> {
        let width = u16::try_from(text.chars().count()).unwrap() * GLYPH_W;
        let mut terminal = Terminal::new(TestBackend::new(width, GROUP_H)).expect("test backend");
        let big = BigText::builder()
            .pixel_size(PixelSize::Quadrant)
            .lines(vec![Line::from(text.to_owned())])
            .build();
        terminal.draw(|frame| frame.render_widget(big, frame.area())).expect("drawing");
        terminal.backend().to_string().lines().map(|l| l.trim_matches('"').to_owned()).collect()
    }

    #[test]
    fn approx_shortens_to_three_figures_when_count_has_more() {
        assert_eq!(approx(0), "0");
        assert_eq!(approx(9_999), "9999");
        assert_eq!(approx(12_345), "12.3k");
        assert_eq!(approx(506_360), "506k");
        assert_eq!(approx(2_350_000), "2.35M");
        assert_eq!(approx(23_500_000), "23.5M");
        // Each unit judged on the printed figure, so these move up rather than
        // reading as "100.0k" and "1000k".
        assert_eq!(approx(99_950), "100k");
        assert_eq!(approx(999_500), "1.00M");
        assert_eq!(approx(9_999_999), "10.0M");
    }

    #[test]
    fn counts_display_big_unique_counts_when_strip_is_wide() {
        let rendered = screen(&busy(), 120, 38);
        let lines: Vec<&str> = rendered.lines().collect();
        let ap = big("32");
        let row = lines.iter().position(|l| l.contains(ap[0].trim_end())).expect("the AP digits");
        for (i, glyphs) in ap.iter().enumerate() {
            assert!(lines[row + i].contains(glyphs.trim_end()), "{glyphs:?} in\n{rendered}");
        }
        // Stacked: the BLE group sits under the AP group.
        for (i, glyphs) in big("8").iter().enumerate() {
            assert!(lines[row + 4 + i].contains(glyphs.trim_end()), "{glyphs:?} in\n{rendered}");
        }
        assert!(lines[row].contains("unique APs"), "{rendered}");
        assert!(lines[row + 2].contains("640 wifi"), "{rendered}");
        assert!(lines[row + 4].contains("unique BLE"), "{rendered}");
        assert!(lines[row + 6].contains("160 bt"), "{rendered}");
        assert!(rendered.contains(" 800 sightings "), "the strip's title: {rendered}");
    }

    #[test]
    fn counts_display_big_unique_counts_side_by_side_when_screen_is_58_columns() {
        let rendered = screen(&busy(), 58, 26);
        let row = rendered.lines().find(|l| l.contains("unique APs")).expect("the AP label");
        assert!(row.contains("unique BLE"), "on one row: {rendered}");
    }

    #[test]
    fn counts_keep_big_digits_when_58_by_26_screen_shows_bluetooth_hint_and_unswept_warning() {
        // The header's longest scan state and an extra fault line must not cost the glance values.
        let mut snapshot = busy();
        snapshot.scan = Some(wartui_proto::fleet::ScanMode::WifiBluetooth);
        snapshot.ble_slot = None;
        snapshot.unreachable = wartui_proto::plan::ChannelPool::All.channels();
        let rendered = screen(&snapshot, 58, 26);
        assert!(rendered.contains("waiting for a second"), "the hint: {rendered}");
        assert!(rendered.contains("unswept"), "the warning: {rendered}");
        let lines: Vec<&str> = rendered.lines().collect();
        let ap = lines.iter().position(|l| l.contains("unique APs")).expect("the AP label");
        assert!(lines[ap].contains("unique BLE"), "side by side: {rendered}");
        for (i, glyphs) in big("32").iter().enumerate() {
            assert!(lines[ap + i].contains(glyphs.trim_end()), "{glyphs:?} in\n{rendered}");
        }
    }

    #[test]
    fn counts_stack_big_unique_counts_when_both_do_not_fit_beside_each_other() {
        // Five-glyph figures for both, so the pair is wider than the narrow strip.
        let mut snapshot = busy();
        snapshot.unique_wifi_aps = 1_230_000;
        snapshot.unique_ble_aps = 12_345;
        let rendered = screen(&snapshot, 58, 30);
        let lines: Vec<&str> = rendered.lines().collect();
        let ap = lines.iter().position(|l| l.contains("unique APs")).expect("the AP label");
        assert!(!lines[ap].contains("unique BLE"), "{rendered}");
        assert!(lines[ap + 4].contains("unique BLE"), "stacked under it: {rendered}");
        for (i, glyphs) in big("1.23M").iter().enumerate() {
            assert!(lines[ap + i].contains(glyphs.trim_end()), "{glyphs:?} in\n{rendered}");
        }
    }

    #[test]
    fn counts_fall_back_to_text_when_strip_is_under_four_rows() {
        let mut terminal = Terminal::new(TestBackend::new(60, 3)).expect("test backend");
        let counts = Counts::new(&busy());
        terminal.draw(|frame| counts.draw(frame, frame.area(), false)).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("~32 APs  ~8 BLE"), "{rendered}");
        assert!(!rendered.contains("unique APs"), "{rendered}");
    }
}
