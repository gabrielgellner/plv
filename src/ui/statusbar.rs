use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    widgets::{Paragraph, Widget},
};

pub struct StatusBar {
    pub file_name: String,
    pub row_offset: usize,
    pub total_rows: usize,
    pub col_offset: usize,
    pub total_cols: usize,
    pub viewport_rows: usize,
}

impl Widget for StatusBar {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let row_end = (self.row_offset + self.viewport_rows).min(self.total_rows);
        let left = format!(
            " {} | Rows {}-{}/{} | Col {}/{}",
            self.file_name,
            self.row_offset + 1,
            row_end,
            self.total_rows,
            self.col_offset + 1,
            self.total_cols,
        );
        let help = " q:quit  j/k:↕  g/G:top/bot  ^d/^u:page  h/l:←→ ";

        let width = area.width as usize;
        let pad = width.saturating_sub(left.len() + help.len());
        let text = format!("{}{}{}", left, " ".repeat(pad), help);

        Paragraph::new(text)
            .style(Style::new().bg(Color::Blue).fg(Color::White).bold())
            .render(area, buf);
    }
}
