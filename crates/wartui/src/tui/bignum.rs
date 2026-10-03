//! Digits three rows tall, drawn in box-drawing characters.
//!
//! The unique counts are the figures a driver reads at a glance, and a terminal has no
//! larger font for one row. These glyphs need no font support and no dependency.

/// The three rows of `text`, an [`approx`](wartui_core::panel::approx) figure, drawn big.
///
/// Digits are three columns wide in a seven-segment style. `.`, `k` and `M` are one
/// column wide on the bottom row, like a subscript. Any other character is blank. One
/// blank column separates characters, and every row has the same width.
pub(super) fn big(text: &str) -> [String; 3] {
    let mut rows = [String::new(), String::new(), String::new()];
    for (i, c) in text.chars().enumerate() {
        let glyph = glyph(c);
        for (row, part) in rows.iter_mut().zip(glyph) {
            if i > 0 {
                row.push(' ');
            }
            row.push_str(part);
        }
    }
    rows
}

/// One character's three rows.
fn glyph(c: char) -> [&'static str; 3] {
    match c {
        '0' => ["┏━┓", "┃ ┃", "┗━┛"],
        '1' => ["  ╻", "  ┃", "  ╹"],
        '2' => ["╺━┓", "┏━┛", "┗━╸"],
        '3' => ["╺━┓", "╺━┫", "╺━┛"],
        '4' => ["╻ ╻", "┗━┫", "  ╹"],
        '5' => ["┏━╸", "┗━┓", "╺━┛"],
        '6' => ["┏━╸", "┣━┓", "┗━┛"],
        '7' => ["╺━┓", "  ┃", "  ╹"],
        '8' => ["┏━┓", "┣━┫", "┗━┛"],
        '9' => ["┏━┓", "┗━┫", "╺━┛"],
        '.' => [" ", " ", "."],
        'k' => [" ", " ", "k"],
        'M' => [" ", " ", "M"],
        _ => [" ", " ", " "],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bignum_draws_each_digit_when_given_all_ten() {
        let rows = big("0123456789");
        assert_eq!(
            rows,
            [
                "┏━┓   ╻ ╺━┓ ╺━┓ ╻ ╻ ┏━╸ ┏━╸ ╺━┓ ┏━┓ ┏━┓",
                "┃ ┃   ┃ ┏━┛ ╺━┫ ┗━┫ ┗━┓ ┣━┓   ┃ ┣━┫ ┗━┫",
                "┗━┛   ╹ ┗━╸ ╺━┛   ╹ ╺━┛ ┗━┛   ╹ ┗━┛ ╺━┛",
            ]
        );
        let widths = rows.map(|row| row.chars().count());
        assert!(widths.iter().all(|&w| w == 39), "{widths:?}");
    }

    #[test]
    fn bignum_puts_point_and_unit_on_bottom_row_when_figure_is_abbreviated() {
        let [top, middle, bottom] = big("12.3k");
        assert_eq!(top, "  ╻ ╺━┓   ╺━┓  ");
        assert_eq!(middle, "  ┃ ┏━┛   ╺━┫  ");
        assert_eq!(bottom, "  ╹ ┗━╸ . ╺━┛ k");
    }
}
