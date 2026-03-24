use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    widgets::{Paragraph, Widget},
};

use super::Theme;

pub struct Prompt<'a> {
    pub buffer: &'a str,
    pub theme: &'a Theme,
}

impl Widget for Prompt<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let text = format!("/{}_", self.buffer);
        Paragraph::new(text)
            .style(Style::new().bg(self.theme.status_bg).fg(self.theme.status_fg))
            .render(area, buf);
    }
}
