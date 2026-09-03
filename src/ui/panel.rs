//! The candidate panel: a short list above the status bar.
//!
//! Not the `?` overlay, which covers the screen and waits for a keypress. This
//! sits *in* the layout while you type, so the table shrinks by its height
//! rather than being hidden behind it, and it is only ever a few lines tall.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

use super::Theme;

/// Space between columns of candidates.
const GAP: usize = 2;
/// At most this many rows, so the panel cannot crowd out the table.
const MAX_ROWS: u16 = 4;

/// The cursor cell shown in full.
///
/// Shares the strip above the status bar with the completion panel, for the
/// same reason: it appears while you are looking at something, takes its rows
/// from the table rather than covering it, and goes away again.
pub struct CellView<'a> {
    pub name: &'a str,
    pub value: &'a str,
    pub theme: &'a Theme,
}

/// Break a value into lines that fit `width`.
///
/// Newlines already in the value are kept — a quoted field can hold them, and
/// they are the author's own line breaks. Everything else is split on width,
/// counting characters rather than bytes.
pub fn wrap(value: &str, width: u16) -> Vec<String> {
    let width = (width as usize).max(1);
    let mut lines = Vec::new();
    for line in value.split('\n') {
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            lines.push(String::new());
            continue;
        }
        for chunk in chars.chunks(width) {
            lines.push(chunk.iter().collect());
        }
    }
    lines
}

/// Rows the cell view wants: a heading plus the wrapped value, within
/// `available`.
pub fn cell_height(value: &str, width: u16, available: u16) -> u16 {
    let wrapped = wrap(value, width).len() as u16;
    (wrapped + 1).min(available.max(2))
}

impl Widget for CellView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        let base = Style::new()
            .bg(self.theme.status_bg)
            .fg(self.theme.status_fg);
        let heading = base.fg(self.theme.header).add_modifier(Modifier::BOLD);

        let wrapped = wrap(self.value, area.width);
        let room = area.height.saturating_sub(1) as usize;
        let shown = wrapped.len().min(room);

        let count = self.value.chars().count();
        let mut lines = vec![Line::from(Span::styled(
            format!(
                "{} — {count} character{}",
                self.name,
                if count == 1 { "" } else { "s" }
            ),
            heading,
        ))];
        lines.extend(
            wrapped
                .iter()
                .take(shown)
                .map(|line| Line::from(Span::styled(line.clone(), base))),
        );
        // Say what was left out rather than stopping in silence.
        if shown < wrapped.len() {
            let last = lines.len() - 1;
            lines[last] = Line::from(Span::styled(
                format!("… {} more lines", wrapped.len() - shown + 1),
                base.add_modifier(Modifier::ITALIC),
            ));
        }

        Paragraph::new(lines).style(base).render(area, buf);
    }
}

pub struct Panel<'a> {
    pub items: &'a [String],
    /// The candidate currently applied to the line, if the list is being
    /// stepped through.
    pub selected: Option<usize>,
    pub theme: &'a Theme,
}

/// How many candidates fit across `width`, and how wide each column is.
fn layout(items: &[String], width: u16) -> (usize, usize) {
    let widest = items.iter().map(|i| i.chars().count()).max().unwrap_or(1);
    let column = widest + GAP;
    let columns = ((width as usize + GAP) / column).max(1);
    (columns, column)
}

/// Rows the panel needs — zero when there is nothing to show, and never more
/// than [`MAX_ROWS`] plus the line that says what was left out.
pub fn height(items: &[String], width: u16) -> u16 {
    if items.is_empty() {
        return 0;
    }
    let (columns, _) = layout(items, width);
    let rows = items.len().div_ceil(columns) as u16;
    if rows > MAX_ROWS {
        MAX_ROWS + 1 // room for the "… n more" line
    } else {
        rows
    }
}

impl Widget for Panel<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if self.items.is_empty() || area.height == 0 {
            return;
        }
        let base = Style::new()
            .bg(self.theme.status_bg)
            .fg(self.theme.status_fg);
        let picked = Style::new()
            .bg(self.theme.completion_bg)
            .fg(self.theme.completion_fg)
            .add_modifier(Modifier::BOLD);

        let (columns, column_width) = layout(self.items, area.width);
        let rows = self.items.len().div_ceil(columns);
        let shown_rows = rows.min(MAX_ROWS as usize);
        let shown = (shown_rows * columns).min(self.items.len());

        let mut lines: Vec<Line> = Vec::with_capacity(shown_rows + 1);
        for row in 0..shown_rows {
            let mut spans = Vec::new();
            for column in 0..columns {
                let Some(index) = index_of(row, column, columns, shown) else {
                    continue;
                };
                let item = &self.items[index];
                let style = if self.selected == Some(index) {
                    picked
                } else {
                    base
                };
                spans.push(Span::styled(item.clone(), style));
                // Pad between items and not after the last one, so a selection
                // at the end of a row does not trail a highlighted block.
                if column + 1 < columns && index + 1 < shown {
                    let pad = column_width.saturating_sub(item.chars().count());
                    spans.push(Span::styled(" ".repeat(pad), base));
                }
            }
            lines.push(Line::from(spans));
        }

        // Say what was left out rather than truncating in silence: a list that
        // quietly stops reads as the whole list.
        if shown < self.items.len() {
            lines.push(Line::from(Span::styled(
                format!("… {} more", self.items.len() - shown),
                base.add_modifier(Modifier::ITALIC),
            )));
        }

        Paragraph::new(lines).style(base).render(area, buf);
    }
}

fn index_of(row: usize, column: usize, columns: usize, shown: usize) -> Option<usize> {
    let index = row * columns + column;
    (index < shown).then_some(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("item{i:02}")).collect()
    }

    #[test]
    fn wrapping_keeps_the_values_own_line_breaks() {
        // A quoted field can hold newlines, and they are the author's.
        assert_eq!(wrap("one\ntwo", 40), ["one", "two"]);
        assert_eq!(wrap("", 40), [""], "an empty value is still a line");
        assert_eq!(wrap("abcdef", 3), ["abc", "def"]);
        assert_eq!(
            wrap("ab\n\ncd", 40),
            ["ab", "", "cd"],
            "a blank line survives"
        );
    }

    #[test]
    fn wrapping_counts_characters_not_bytes() {
        // Three characters, six bytes: a byte-wise split would cut one in half.
        assert_eq!(wrap("éàü", 3), ["éàü"]);
        assert_eq!(wrap("éàü", 2), ["éà", "ü"]);
    }

    #[test]
    fn a_narrow_panel_does_not_divide_by_zero() {
        assert_eq!(wrap("abc", 0), ["a", "b", "c"]);
    }

    #[test]
    fn the_cell_view_asks_for_a_heading_plus_its_lines() {
        assert_eq!(cell_height("one line", 40, 10), 2, "heading and one line");
        assert_eq!(cell_height("abcdef", 3, 10), 3, "heading and two");
        // Never more than it is given.
        assert_eq!(cell_height(&"x".repeat(400), 10, 6), 6);
    }

    #[test]
    fn a_value_too_long_for_the_panel_says_how_much_is_missing() {
        let theme = Theme::catppuccin_mocha();
        let value = "x".repeat(200);
        // Wide enough for the heading; the value is what should be cut.
        let area = Rect::new(0, 0, 30, 4);
        let mut buf = Buffer::empty(area);
        CellView {
            name: "note",
            value: &value,
            theme: &theme,
        }
        .render(area, &mut buf);

        let lines: Vec<String> = (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        assert!(lines[0].contains("note"), "{lines:?}");
        assert!(lines[0].contains("200 characters"), "{lines:?}");
        assert!(
            lines[area.height as usize - 1].contains("more lines"),
            "{lines:?}"
        );
    }

    #[test]
    fn nothing_to_show_takes_no_room() {
        assert_eq!(height(&[], 80), 0);
    }

    #[test]
    fn candidates_pack_across_the_width() {
        // "item00" is 6 wide, so 8 per column; 40 wide fits 5 across.
        assert_eq!(height(&items(5), 40), 1);
        assert_eq!(height(&items(6), 40), 2);
    }

    #[test]
    fn a_long_list_is_capped_with_room_to_say_so() {
        let tall = height(&items(200), 40);
        assert_eq!(tall, MAX_ROWS + 1, "capped, plus the line that owns up");
    }

    #[test]
    fn a_narrow_panel_still_shows_one_column() {
        let (columns, _) = layout(&items(3), 2);
        assert_eq!(columns, 1, "never zero, however narrow");
    }

    #[test]
    fn what_is_left_out_is_said_out_loud() {
        let theme = Theme::catppuccin_mocha();
        let all = items(200);
        let area = Rect::new(0, 0, 40, height(&all, 40));
        let mut buf = Buffer::empty(area);
        Panel {
            items: &all,
            selected: None,
            theme: &theme,
        }
        .render(area, &mut buf);

        let last: String = (0..area.width)
            .map(|x| buf[(x, area.height - 1)].symbol())
            .collect();
        assert!(last.contains("more"), "{last:?}");
    }
}
