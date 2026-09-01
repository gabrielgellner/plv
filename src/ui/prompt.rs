use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

use super::Theme;

/// The one-line text field that takes over the status bar: search patterns,
/// cell edits and `:` commands all type into it.
pub struct Prompt<'a> {
    /// What the line is for — `/`, `:`, or the name of the column being edited.
    pub prefix: &'a str,
    pub buffer: &'a str,
    /// Character the caret sits on. Past the end it draws as a block after the
    /// last character, which is where an empty field starts.
    pub cursor: usize,
    pub theme: &'a Theme,
}

impl Widget for Prompt<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let base = Style::new()
            .bg(self.theme.status_bg)
            .fg(self.theme.status_fg);
        let caret = base.add_modifier(Modifier::REVERSED);

        let chars: Vec<char> = self.buffer.chars().collect();
        let mut spans = vec![Span::styled(self.prefix.to_string(), base)];
        for (i, ch) in chars.iter().enumerate() {
            let style = if i == self.cursor { caret } else { base };
            spans.push(Span::styled(ch.to_string(), style));
        }
        if self.cursor >= chars.len() {
            spans.push(Span::styled(" ", caret));
        }

        Paragraph::new(Line::from(spans))
            .style(base)
            .render(area, buf);
    }
}
