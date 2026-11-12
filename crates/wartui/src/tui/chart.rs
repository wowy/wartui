//! Wi-Fi sightings per channel, one bar per channel of the pool the bridge holds, in scan-table
//! order, so a band the fleet is not hearing shows as a gap where its bars should be.
//!
//! Only the pool's channels are charted because the fleet sweeps no others: under `Us` or `Eu` a
//! bar for an unswept channel would take a column and say nothing. A sighting on a channel
//! outside the pool, heard from a neighbor or counted before a pool change, has no bar and still
//! counts in the strip's title. Before the bridge reports a pool the chart shows all of `All`.
//!
//! `tui-bar-graph` draws the bars and nothing else, so the axis and the channel labels are drawn
//! here by hand, aligned to the columns the bars are given. Channel 14 has no bar because it is in
//! no pool. Bluetooth is not charted, since a Bluetooth sighting has no channel.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use tui_bar_graph::{BarGraph, BarStyle, ColorMode};
use wartui_proto::plan::{ChannelPool, NUM_SCAN_CHANNELS, SCAN_CHANNELS};

use super::counts::approx;

/// The channels each band starts at: 2.4 GHz, then the three 5 GHz runs the table holds.
const BAND_STARTS: [u8; 4] = [1, 36, 100, 149];

/// One charted channel: its number, its sightings and whether a band starts at it.
struct Bar {
    channel: u8,
    count: u64,
    starts_band: bool,
}

/// One bar per channel of `pool`, in table order. A channel outside it has no bar, whatever it
/// counts, and channel 14 is in no pool.
fn bars(counts: &[u64; NUM_SCAN_CHANNELS as usize], pool: ChannelPool) -> Vec<Bar> {
    (0u8..)
        .zip(SCAN_CHANNELS.iter().zip(counts))
        .filter(|&(i, _)| pool.contains(i))
        .map(|(_, (&channel, &count))| Bar {
            channel,
            count,
            starts_band: BAND_STARTS.contains(&channel),
        })
        .collect()
}

/// The columns the bars are laid out in: what each column holds, from 0 to 1, and the first
/// column of each bar.
struct Columns {
    values: Vec<f64>,
    first: Vec<usize>,
    cell: usize,
    bar: usize,
}

/// Lay `bars` out in `width` columns: `cell` columns per channel, a bar `cell - 1` wide and a
/// blank column after it, and one more blank column between bands. Below the width all that
/// takes, each channel gets one column and the bands touch.
fn columns(bars: &[Bar], width: u16, highest: u64) -> Columns {
    let n = bars.len();
    let gaps = bars.iter().skip(1).filter(|b| b.starts_band).count();
    let width = usize::from(width);
    let banded = width >= n + gaps;
    let cell = if banded { ((width - gaps) / n).max(1) } else { 1 };
    let (bar, gap) = if cell >= 2 { (cell - 1, 1) } else { (1, 0) };
    let scale = highest.max(1) as f64;
    let mut values = Vec::with_capacity(width);
    let mut first = Vec::with_capacity(n);
    for (i, b) in bars.iter().enumerate() {
        if banded && b.starts_band && i > 0 {
            values.push(0.0);
        }
        first.push(values.len());
        values.extend(std::iter::repeat_n(b.count as f64 / scale, bar));
        values.extend(std::iter::repeat_n(0.0, gap));
    }
    Columns { values, first, cell, bar }
}

/// Draw the chart of `pool`'s channels into `area`: the axis at the left, the bars, and the
/// channel labels under them. The axis tops at the loudest charted channel.
///
/// It sheds by height, keeping three bar rows: the three label rows drop to one, then that one
/// goes, then below two rows the axis.
pub(super) fn draw_chart(
    frame: &mut Frame<'_>,
    area: Rect,
    counts: &[u64; NUM_SCAN_CHANNELS as usize],
    pool: ChannelPool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let bars = bars(counts, pool);
    let highest = bars.iter().map(|b| b.count).max().unwrap_or(0);
    let top = approx(highest.max(1));
    let label_w = u16::try_from(top.len()).unwrap_or(u16::MAX).max(3);
    let gutter = if area.height >= 2 { label_w + 1 } else { 0 };
    let plot_w = area.width.saturating_sub(gutter);
    let Columns { values, first, cell, bar } = columns(&bars, plot_w, highest);
    // Three stacked digits under each channel when it has a column to spare and the rows allow;
    // one row naming each band otherwise.
    let label_rows = match area.height {
        h if cell >= 2 && h >= 6 => 3,
        h if h >= 4 => 1,
        _ => 0,
    };
    let bar_rows = area.height - label_rows;
    let plot = Rect { x: area.x + gutter, y: area.y, width: plot_w, height: bar_rows };

    if gutter > 0 {
        let w = usize::from(label_w);
        let axis: Vec<Line<'static>> = (0..bar_rows)
            .map(|row| match row {
                0 => format!("{top:>w$}┤"),
                r if r + 1 == bar_rows => format!("{:>w$}┤", 0),
                _ => format!("{:>w$}│", ""),
            })
            .map(Line::from)
            .collect();
        frame.render_widget(Paragraph::new(axis), Rect { width: gutter, height: bar_rows, ..area });
    }
    if plot.width > 0 && highest > 0 {
        let graph = BarGraph::new(values)
            .with_bar_style(BarStyle::Solid)
            .with_color_mode(ColorMode::VerticalGradient)
            .with_gradient(colorgrad::preset::viridis())
            .with_min(0.0)
            .with_max(1.0);
        frame.render_widget(graph, plot);
    }
    if label_rows > 0 && plot.width > 0 {
        let lines = labels(&bars, &first, bar, label_rows, usize::from(plot.width));
        let under = Rect { y: area.y + bar_rows, height: label_rows, ..plot };
        frame.render_widget(Paragraph::new(lines), under);
    }
}

/// The label rows under the bars, `width` columns wide, for bars starting at the columns in
/// `first` and `bar` columns wide.
///
/// Three rows write every channel's number downward, hundreds to ones, in its bar's first
/// column. One row names the first channel of each band under its first bar, and the last
/// channel ending under the last bar.
fn labels(
    bars: &[Bar],
    first: &[usize],
    bar: usize,
    rows: u16,
    width: usize,
) -> Vec<Line<'static>> {
    let mut grid = vec![vec![' '; width]; usize::from(rows)];
    if rows == 3 {
        for (b, &x) in bars.iter().zip(first) {
            let digits = format!("{:>3}", b.channel);
            for (row, c) in grid.iter_mut().zip(digits.chars()) {
                if let Some(cell) = row.get_mut(x) {
                    *cell = c;
                }
            }
        }
    } else {
        let mut marks: Vec<(usize, String)> = bars
            .iter()
            .zip(first)
            .filter(|(b, _)| b.starts_band)
            .map(|(b, &x)| (x, b.channel.to_string()))
            .collect();
        if let (Some(last), Some(&x)) = (bars.last(), first.last()) {
            let text = last.channel.to_string();
            let end = x + bar;
            marks.push((end.saturating_sub(text.len()), text));
        }
        let mut free = 0;
        for (x, text) in marks {
            // A label that would touch the one before it, or run off the edge, is left out.
            if x < free || x + text.len() > width {
                continue;
            }
            for (cell, c) in grid[0][x..].iter_mut().zip(text.chars()) {
                *cell = c;
            }
            free = x + text.len() + 1;
        }
    }
    grid.into_iter().map(|row| Line::from(row.into_iter().collect::<String>())).collect()
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    /// The chart for `counts` drawn alone at `width`×`height`, as rows of text.
    fn chart(
        counts: &[u64; NUM_SCAN_CHANNELS as usize],
        pool: ChannelPool,
        width: u16,
        height: u16,
    ) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test backend");
        terminal.draw(|frame| draw_chart(frame, frame.area(), counts, pool)).expect("drawing");
        terminal.backend().to_string().lines().map(|l| l.trim_matches('"').to_owned()).collect()
    }

    fn sample() -> [u64; NUM_SCAN_CHANNELS as usize] {
        let mut counts = [0; NUM_SCAN_CHANNELS as usize];
        counts[0] = 280;
        counts[5] = 310;
        counts[10] = 50;
        counts[41] = 5;
        counts
    }

    #[test]
    fn chart_labels_every_channel_when_width_allows() {
        // 4 columns of axis and 85 of bars: two columns a channel.
        let rows = chart(&sample(), ChannelPool::All, 89, 9);
        let [hundreds, tens, ones] = [&rows[6], &rows[7], &rows[8]];
        assert!(ones.starts_with("    1 2 3 4 5 6 7 8 9 0 1 2 3  6 0 4 8"), "{rows:#?}");
        assert!(tens.starts_with("                      1 1 1 1  3 4 4 4"), "{rows:#?}");
        assert!(hundreds.contains('1'), "{rows:#?}");
        assert!(rows[0].starts_with("310┤"), "{rows:#?}");
        assert!(rows[5].starts_with("  0┤"), "{rows:#?}");
        assert!(rows[2].starts_with("   │"), "{rows:#?}");
    }

    #[test]
    fn chart_names_each_band_when_one_column_per_channel() {
        let rows = chart(&sample(), ChannelPool::All, 56, 4);
        let labels = &rows[3];
        assert!(
            labels.starts_with("    1             36       100          149  177"),
            "{rows:#?}"
        );
    }

    #[test]
    fn chart_names_each_band_when_two_columns_per_channel_but_four_rows() {
        let rows = chart(&sample(), ChannelPool::All, 89, 4);
        let labels = &rows[3];
        let at: Vec<usize> = ["1 ", " 36 ", " 100 ", " 149 ", " 177"]
            .iter()
            .map(|label| labels.find(label).unwrap_or_else(|| panic!("{label:?}: {rows:#?}")))
            .collect();
        assert!(at.is_sorted(), "in band order: {rows:#?}");
        assert!(!labels.contains('█'), "a label row, not bars: {rows:#?}");
        assert!(rows[0].contains('█'), "over three bar rows: {rows:#?}");
    }

    #[test]
    fn chart_omits_channel_14() {
        let charted: Vec<u8> =
            bars(&sample(), ChannelPool::All).iter().map(|b| b.channel).collect();
        assert_eq!(charted.len(), 41);
        assert!(!charted.contains(&14));
        // Every bar after 13 is a 5 GHz channel, in table order.
        assert_eq!(&charted[12..14], &[13, 36]);
        assert_eq!(charted.last(), Some(&177));
    }

    #[test]
    fn chart_charts_only_pool_channels_when_pool_is_us() {
        let charted: Vec<u8> = bars(&sample(), ChannelPool::Us).iter().map(|b| b.channel).collect();
        assert_eq!(charted.len(), 36);
        for hidden in [12, 13, 14, 169, 173, 177] {
            assert!(!charted.contains(&hidden), "{hidden}: {charted:?}");
        }
        assert_eq!(charted.last(), Some(&165));
    }

    #[test]
    fn chart_charts_only_pool_channels_when_pool_is_eu() {
        let charted: Vec<u8> = bars(&sample(), ChannelPool::Eu).iter().map(|b| b.channel).collect();
        assert_eq!(charted.len(), 32);
        for hidden in [14, 144, 149, 153, 157, 161, 165, 169, 173, 177] {
            assert!(!charted.contains(&hidden), "{hidden}: {charted:?}");
        }
        assert_eq!(charted.last(), Some(&140));
    }

    #[test]
    fn chart_names_last_pool_channel_when_pool_is_eu() {
        // Two band gaps, not three: EU has no run at 149.
        let rows = chart(&sample(), ChannelPool::Eu, 56, 4);
        let labels = &rows[3];
        assert!(labels.starts_with("    1             36       100     140"), "{rows:#?}");
        assert!(!labels.contains("149"), "{rows:#?}");
    }

    #[test]
    fn chart_scales_to_charted_channels_when_loudest_is_outside_pool() {
        let mut counts = sample();
        counts[41] = 5000;
        let rows = chart(&counts, ChannelPool::Us, 89, 9);
        assert!(rows[0].starts_with("310┤"), "{rows:#?}");
    }

    #[test]
    fn chart_draws_bars_only_when_area_is_tiny() {
        let rows = chart(&sample(), ChannelPool::All, 30, 1);
        assert!(!rows[0].contains('┤'), "no axis: {rows:?}");
        assert!(rows[0].contains('█'), "but bars: {rows:?}");
        for pool in [ChannelPool::All, ChannelPool::Eu, ChannelPool::Us] {
            for (width, height) in [(0, 0), (1, 1), (3, 2), (10, 3), (44, 5)] {
                let mut terminal =
                    Terminal::new(TestBackend::new(width, height)).expect("test backend");
                terminal
                    .draw(|frame| draw_chart(frame, frame.area(), &sample(), pool))
                    .expect("drawing");
            }
        }
    }

    #[test]
    fn chart_draws_no_bars_when_nothing_was_heard() {
        let rows = chart(&[0; NUM_SCAN_CHANNELS as usize], ChannelPool::All, 89, 9);
        assert!(rows.iter().all(|row| !row.contains('█')), "{rows:#?}");
        assert!(rows[0].starts_with("  1┤"), "{rows:#?}");
    }
}
