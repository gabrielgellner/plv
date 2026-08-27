use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders, Cell, Row, Table, Widget},
};

use super::Theme;

/// A scrollable, single-selection list used for the lake catalog: one level
/// shows tables, the next shows the data files of a table.
pub struct Browser<'a> {
    pub title: String,
    pub headers: &'a [&'a str],
    pub widths: &'a [Constraint],
    pub rows: &'a [Vec<String>],
    pub selected: usize,
    pub offset: usize,
    pub theme: &'a Theme,
}

impl Browser<'_> {
    /// Rows of list content that fit in a pane `height` tall
    /// (2 border lines + 1 header row of overhead).
    pub fn viewport_rows(height: u16) -> usize {
        (height as usize).saturating_sub(3).max(1)
    }
}

impl Widget for Browser<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(self.theme.border))
            .title(self.title.clone());

        let visible = Self::viewport_rows(area.height);
        let end = (self.offset + visible).min(self.rows.len());
        let slice = self.rows.get(self.offset..end).unwrap_or(&[]);

        let header = Row::new(
            self.headers
                .iter()
                .map(|h| Cell::from(*h).style(Style::new().fg(self.theme.header))),
        );

        let body = slice.iter().enumerate().map(|(i, cells)| {
            let absolute = self.offset + i;
            let style = if absolute == self.selected {
                Style::new()
                    .bg(self.theme.cursor_bg)
                    .fg(self.theme.cursor_fg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            Row::new(cells.iter().map(|c| Cell::from(c.clone()))).style(style)
        });

        Table::new(body, self.widths)
            .header(header)
            .block(block)
            .column_spacing(2)
            .render(area, buf);
    }
}

/// Cursor + scroll state for a `Browser` list.
#[derive(Default)]
pub struct BrowserState {
    pub selected: usize,
    pub offset: usize,
}

impl BrowserState {
    pub fn move_by(&mut self, delta: isize, len: usize) {
        if len == 0 {
            self.selected = 0;
            return;
        }
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, len as isize - 1) as usize;
    }

    pub fn go_to(&mut self, index: usize, len: usize) {
        self.selected = index.min(len.saturating_sub(1));
    }

    /// Keep `selected` inside the visible window of `visible` rows.
    pub fn clamp_scroll(&mut self, visible: usize) {
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + visible {
            self.offset = self.selected + 1 - visible;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_by_clamps_to_bounds() {
        let mut s = BrowserState::default();
        s.move_by(5, 3);
        assert_eq!(s.selected, 2);
        s.move_by(-10, 3);
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn move_by_on_empty_list_stays_at_zero() {
        let mut s = BrowserState::default();
        s.move_by(1, 0);
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn clamp_scroll_follows_cursor_both_ways() {
        let mut s = BrowserState { selected: 9, ..Default::default() };
        s.clamp_scroll(5);
        assert_eq!(s.offset, 5); // cursor at bottom of window

        s.selected = 2;
        s.clamp_scroll(5);
        assert_eq!(s.offset, 2); // scrolled back up
    }
}
