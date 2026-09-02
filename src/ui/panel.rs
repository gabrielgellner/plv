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
