use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    widgets::{Paragraph, Widget},
};

use super::Theme;

const SPINNER: &[&str] = &[".   ", "..  ", "... ", "...."];

pub struct StatusBar<'a> {
    pub file_name: String,
    pub cursor_row: usize,
    pub total_rows: usize,
    pub col_offset: usize,
    pub total_cols: usize,
    pub message: Option<String>,
    pub pending_num: String,
    pub pending_z: bool,
    pub theme: &'a Theme,
    /// Active search: `(pattern, current_1based, total, complete)`.
    /// When `complete` is false the scan is still running and total may grow.
    pub search_info: Option<(String, usize, usize, bool)>,
    /// Incremented each frame while a search is in progress; drives the spinner.
    pub spinner_tick: usize,
}

impl Widget for StatusBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if let Some(msg) = self.message {
            let text = format!(" {msg}");
            Paragraph::new(text)
                .style(Style::new().bg(self.theme.message_bg).fg(self.theme.message_fg))
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
            left.push_str(&format!("  [{}]", self.pending_num));
        }

        if let Some((pat, cur, total, complete)) = &self.search_info {
            if *complete {
                if *total == 0 {
                    left.push_str(&format!("  /{pat}  [no matches]"));
                } else {
                    left.push_str(&format!("  /{pat}  [{cur}/{total}]"));
                }
            } else {
                let spin = SPINNER[(self.spinner_tick / 4) % SPINNER.len()];
                if *total == 0 {
                    left.push_str(&format!("  /{pat}  [{spin}]"));
                } else {
                    left.push_str(&format!("  /{pat}  [{cur}/{total} {spin}]"));
                }
            }
        }

        let help = " q  j/k:↕  g/G:top/bot  ^d/^u:page  h/l:←→  zz/zt/zb ";
        let width = area.width as usize;
        let pad = width.saturating_sub(left.len() + help.len());
        let text = format!("{}{}{}", left, " ".repeat(pad), help);

        Paragraph::new(text)
            .style(Style::new().bg(self.theme.status_bg).fg(self.theme.status_fg))
            .render(area, buf);
    }
}
