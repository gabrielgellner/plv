use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    widgets::{Paragraph, Widget},
};

pub struct StatusBar {
    pub file_name: String,
    pub cursor_row: usize,
    pub total_rows: usize,
    pub col_offset: usize,
    pub total_cols: usize,
    pub message: Option<String>,
    pub pending_num: String,
    pub pending_z: bool,
}

impl Widget for StatusBar {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if let Some(msg) = self.message {
            let text = format!(" {msg}");
            Paragraph::new(text)
                .style(Style::new().bg(Color::Rgb(160, 100, 0)).fg(Color::White))
                .render(area, buf);
            return;
        }

        let mut left = format!(
            " {} | Row {}/{} | Col {}/{}",
            self.file_name,
            self.cursor_row + 1,
            self.total_rows,
            self.col_offset + 1,
            self.total_cols,
        );

        if self.pending_z {
            left.push_str("  z-");
        } else if !self.pending_num.is_empty() {
            left.push_str(&format!("  {}G?", self.pending_num));
        }

        let help = " q  j/k:↕  g/G:top/bot  ^d/^u:page  h/l:←→  zz/zt/zb ";
        let width = area.width as usize;
        let pad = width.saturating_sub(left.len() + help.len());
        let text = format!("{}{}{}", left, " ".repeat(pad), help);

        Paragraph::new(text)
            .style(Style::new().bg(Color::Blue).fg(Color::White))
            .render(area, buf);
    }
}
