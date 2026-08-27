use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

use super::Theme;

const SPINNER: &[&str] = &[".   ", "..  ", "... ", "...."];
const SORT_WORD: &str = "Sorting";

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
    /// `Some(tick)` while a background sort is running; drives the sort animation.
    pub sort_tick: Option<usize>,
    /// Key-help text for the right-hand side; varies by mode.
    pub help: &'static str,
}

impl Widget for StatusBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let base = Style::new().bg(self.theme.status_bg).fg(self.theme.status_fg);

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

        let search_text = if let Some((pat, cur, total, complete)) = &self.search_info {
            if *complete {
                if *total == 0 {
                    format!("  /{pat}  [no matches]")
                } else {
                    format!("  /{pat}  [{cur}/{total}]")
                }
            } else {
                let spin = SPINNER[(self.spinner_tick / 4) % SPINNER.len()];
                if *total == 0 {
                    format!("  /{pat}  [{spin}]")
                } else {
                    format!("  /{pat}  [{cur}/{total} {spin}]")
                }
            }
        } else {
            String::new()
        };

        let help = self.help;
        let width = area.width as usize;

        if let Some(tick) = self.sort_tick {
            // "  Sorting" with one cycling bold character.
            let sort_prefix = "  ";
            let bold_idx = (tick / 3) % SORT_WORD.len();

            let left_len = left.len()
                + sort_prefix.len()
                + SORT_WORD.len()
                + search_text.len();
            let pad = width.saturating_sub(left_len + help.len());

            let mut spans: Vec<Span<'static>> = vec![
                Span::styled(left, base),
                Span::styled(sort_prefix, base),
            ];
            for (i, ch) in SORT_WORD.chars().enumerate() {
                let style = if i == bold_idx {
                    base.add_modifier(Modifier::BOLD)
                } else {
                    base
                };
                spans.push(Span::styled(ch.to_string(), style));
            }
            spans.push(Span::styled(search_text, base));
            spans.push(Span::styled(" ".repeat(pad), base));
            spans.push(Span::styled(help, base));

            Paragraph::new(Line::from(spans)).render(area, buf);
        } else {
            let pad = width.saturating_sub(left.len() + search_text.len() + help.len());
            let text = format!("{left}{search_text}{}{help}", " ".repeat(pad));
            Paragraph::new(text).style(base).render(area, buf);
        }
    }
}
