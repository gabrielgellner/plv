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
/// Fallback when the full key help will not fit beside the position readout.
const MIN_HELP: &str = " ?:help ";

pub struct StatusBar<'a> {
    pub file_name: String,
    pub cursor_row: usize,
    pub total_rows: usize,
    /// The column the readout names: the cursor's, or the leftmost
    /// visible one when there is no column cursor.
    pub col_position: usize,
    pub total_cols: usize,
    pub message: Option<String>,
    /// Cells edited but not yet written. Shown as `[+n]` beside the file name.
    pub dirty: usize,
    /// Size of the visual selection as `(rows, columns)`, when there is one.
    pub selection: Option<(usize, usize)>,
    /// What the active view is doing, when it is doing anything.
    pub view: Option<String>,
    /// A filter scan is still running, so the row count is still growing.
    pub filtering: bool,
    pub pending_num: String,
    /// A multi-key prefix waiting for its second key, shown as `g-` or `z-`.
    pub pending_prefix: Option<char>,
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
        let base = Style::new()
            .bg(self.theme.status_bg)
            .fg(self.theme.status_fg);

        if let Some(msg) = self.message {
            let text = format!(" {msg}");
            Paragraph::new(text)
                .style(
                    Style::new()
                        .bg(self.theme.message_bg)
                        .fg(self.theme.message_fg),
                )
                .render(area, buf);
            return;
        }

        let dirty = if self.dirty > 0 {
            format!(" [+{}]", self.dirty)
        } else {
            String::new()
        };

        let mut left = format!(
            " {}{dirty} | Row {}/{} | Col {}/{}",
            self.file_name,
            self.cursor_row + 1,
            self.total_rows,
            self.col_position + 1,
            self.total_cols,
        );

        if let Some(view) = &self.view {
            left.push_str(&format!("  {view}"));
            if self.filtering {
                // The row count beside it is still climbing; say so rather
                // than let it look like the final answer.
                left.push_str(SPINNER[(self.spinner_tick / 4) % SPINNER.len()].trim_end());
            }
        }

        if let Some((rows, cols)) = self.selection {
            left.push_str(&format!("  {rows}\u{d7}{cols} sel"));
        }

        if let Some(prefix) = self.pending_prefix {
            left.push_str(&format!("  {prefix}-"));
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

        let width = area.width as usize;

        // Position and search state matter more than the key hints, and the
        // `?` overlay is the real reference — so shrink, then drop, the help
        // rather than truncating what is to its left.
        let fixed = display_width(&left) + display_width(&search_text);
        let help = if fixed + display_width(self.help) <= width {
            self.help
        } else if fixed + display_width(MIN_HELP) <= width {
            MIN_HELP
        } else {
            ""
        };

        if let Some(tick) = self.sort_tick {
            // "  Sorting" with one cycling bold character.
            let sort_prefix = "  ";
            let bold_idx = (tick / 3) % SORT_WORD.len();

            let left_len = display_width(&left)
                + sort_prefix.len()
                + SORT_WORD.chars().count()
                + display_width(&search_text);
            let pad = width.saturating_sub(left_len + display_width(help));

            let mut spans: Vec<Span<'static>> =
                vec![Span::styled(left, base), Span::styled(sort_prefix, base)];
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
            let pad = width.saturating_sub(fixed + display_width(help));
            let text = format!("{left}{search_text}{}{help}", " ".repeat(pad));
            Paragraph::new(text).style(base).render(area, buf);
        }
    }
}

/// Terminal cells a string occupies. Close enough for the status bar, whose
/// only non-ASCII content is single-width arrows.
fn display_width(s: &str) -> usize {
    s.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_width_counts_chars_not_bytes() {
        assert_eq!(display_width(" j/k:\u{2195} "), 7);
    }
}
