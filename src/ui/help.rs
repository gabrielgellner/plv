use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget},
};

use super::Theme;

/// One group of key bindings: a heading and its `(keys, action)` rows.
pub type Section<'a> = (&'a str, &'a [(&'a str, &'a str)]);

/// Centred popup listing the key bindings for the current screen.
///
/// The status bar can only ever show a handful of keys before it clips, so
/// this is the authoritative in-app reference.
pub struct Help<'a> {
    pub sections: &'a [Section<'a>],
    pub theme: &'a Theme,
}

impl Help<'_> {
    /// Lines a section occupies: heading + rows + trailing blank.
    fn section_height(section: &Section<'_>) -> usize {
        section.1.len() + 2
    }

    /// Split sections into two balanced columns, keeping each section whole.
    fn split(&self) -> (&[Section<'_>], &[Section<'_>]) {
        let total: usize = self.sections.iter().map(Help::section_height).sum();
        let mut used = 0;
        let mut at = self.sections.len();
        for (i, section) in self.sections.iter().enumerate() {
            if used * 2 >= total {
                at = i;
                break;
            }
            used += Help::section_height(section);
        }
        self.sections.split_at(at)
    }

    /// Width a column needs: the widest heading, or the widest
    /// `  keys  action` row.
    fn column_width(sections: &[Section<'_>], key_width: usize) -> usize {
        sections
            .iter()
            .flat_map(|(heading, entries)| {
                std::iter::once(heading.chars().count()).chain(
                    entries
                        .iter()
                        .map(|(_, action)| 4 + key_width + action.chars().count()),
                )
            })
            .max()
            .unwrap_or(0)
    }

    fn render_column(&self, sections: &[Section<'_>], key_width: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for (heading, entries) in sections {
            lines.push(Line::from(Span::styled(
                heading.to_string(),
                Style::new()
                    .fg(self.theme.header)
                    .add_modifier(Modifier::BOLD),
            )));
            for (keys, action) in *entries {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {keys:key_width$}  "),
                        Style::new().fg(self.theme.match_bg),
                    ),
                    Span::raw(action.to_string()),
                ]));
            }
            lines.push(Line::raw(""));
        }
        lines
    }
}

impl Widget for Help<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (left, right) = self.split();
        let key_width = self
            .sections
            .iter()
            .flat_map(|(_, entries)| entries.iter())
            .map(|(keys, _)| keys.chars().count())
            .max()
            .unwrap_or(8);

        let body_height = left
            .iter()
            .map(Help::section_height)
            .sum::<usize>()
            .max(right.iter().map(Help::section_height).sum::<usize>());

        // Size to the content, but never overflow the terminal.
        const GAP: usize = 2;
        let left_width = Help::column_width(left, key_width);
        let right_width = Help::column_width(right, key_width);
        let width = ((left_width + GAP + right_width + 2) as u16)
            .min(area.width)
            .max(20);
        let height = ((body_height + 2) as u16).min(area.height).max(3);
        let popup = Rect {
            x: area.x + (area.width.saturating_sub(width)) / 2,
            y: area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
        };

        Clear.render(popup, buf);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(self.theme.border))
            .title(" Keys — any key to close ");
        let inner = block.inner(popup);
        block.render(popup, buf);

        let [left_area, _, right_area] = Layout::horizontal([
            Constraint::Length(left_width as u16),
            Constraint::Length(GAP as u16),
            Constraint::Min(0),
        ])
        .areas(inner);

        Paragraph::new(self.render_column(left, key_width)).render(left_area, buf);
        Paragraph::new(self.render_column(right, key_width)).render(right_area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &[(&str, &str)] = &[("j", "down"), ("k", "up")];
    const B: &[(&str, &str)] = &[("q", "quit")];

    #[test]
    fn split_balances_columns() {
        let theme = Theme::catppuccin_mocha();
        let sections: &[Section] = &[("Move", A), ("Other", B)];
        let help = Help {
            sections,
            theme: &theme,
        };
        let (left, right) = help.split();
        assert_eq!(left.len(), 1);
        assert_eq!(right.len(), 1);
    }

    #[test]
    fn split_keeps_single_section_in_one_column() {
        let theme = Theme::catppuccin_mocha();
        let sections: &[Section] = &[("Move", A)];
        let help = Help {
            sections,
            theme: &theme,
        };
        let (left, right) = help.split();
        assert_eq!(left.len(), 1);
        assert!(right.is_empty());
    }

    #[test]
    fn column_width_fits_the_widest_row() {
        // 2 leading spaces + key column + 2 spaces + "down"
        assert_eq!(Help::column_width(&[("Move", A)], 3), 4 + 3 + 4);
    }
}
