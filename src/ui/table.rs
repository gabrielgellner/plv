use polars::prelude::*;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Row, Table, Widget},
};

use crate::search::SearchState;

use super::{SelectionMode, Theme};

const MIN_COL_WIDTH: usize = 3;
/// Spacing baked into each data column's constraint width so the cursor bg fills gaps.
const COLUMN_SPACING: usize = 4;
/// A column may consume at most this fraction of the available terminal width.
const MAX_COL_FRAC: f32 = 0.3;

pub struct DataTable<'a> {
    pub df: &'a DataFrame,
    pub col_offset: usize,
    pub cursor_col: usize,
    pub row_offset: usize,
    pub cursor_row: usize,
    pub selection_mode: SelectionMode,
    pub theme: &'a Theme,
    pub search: Option<&'a SearchState>,
    /// Which column to restrict search highlights to. `None` = all columns.
    /// Decoupled from `cursor_col` so highlights stay on the searched column
    /// even after the cursor moves away.
    pub search_col: Option<usize>,
    /// Written with the last fully-visible column index after each render so
    /// the app layer can scroll when the column cursor reaches the right edge.
    pub last_vis_col_out: &'a std::cell::Cell<usize>,
}

/// Dynamic row number column width based on the current viewport position.
///
/// Rounds the lookahead horizon up to the next power of 10 so the column only
/// widens at order-of-magnitude boundaries (1→10→100→…), not every digit.
fn row_num_width(row_offset: usize, viewport_rows: usize) -> usize {
    let horizon = (row_offset + viewport_rows * 3).max(99);
    let mut p: usize = 10;
    while p <= horizon {
        p *= 10;
    }
    // p.to_string().len() - 1 = digits needed to represent (p - 1)
    (p.to_string().len() - 1) + 2 // +2: one left-padding space + the │ char
}

fn natural_col_width(col: &Column) -> usize {
    let header_w = col.name().len();
    let data_w = (0..col.len())
        .map(|i| col.get(i).map(|v| format!("{v}").len()).unwrap_or(0))
        .max()
        .unwrap_or(0);
    header_w.max(data_w).max(MIN_COL_WIDTH)
}

/// csvlens-style width redistribution.
///
/// After capping every column at `max_w`, any slack (unused terminal space) is
/// given back to the capped columns, narrowest first, so they can grow back
/// toward their natural width.
fn redistribute(mut widths: Vec<usize>, naturals: &[usize], slack: usize) -> Vec<usize> {
    if slack == 0 {
        return widths;
    }
    let mut clipped: Vec<usize> = (0..widths.len())
        .filter(|&i| naturals[i] > widths[i])
        .collect();
    clipped.sort_by_key(|&i| naturals[i]);

    let mut rem = slack;
    for (order, &ci) in clipped.iter().enumerate() {
        let left = clipped.len() - order;
        let alloc = rem / left;
        let grow = alloc.min(naturals[ci] - widths[ci]);
        widths[ci] += grow;
        rem -= grow;
    }
    widths
}

/// Split `text` into styled spans, highlighting regex match ranges.
/// The `base_style` is applied to non-matching text; `match_style` to matches.
fn highlight_cell(text: &str, state: &SearchState, base_style: Style, match_style: Style) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut last = 0;
    for mat in state.query.regex.find_iter(text) {
        if mat.start() > last {
            spans.push(Span::styled(text[last..mat.start()].to_string(), base_style));
        }
        spans.push(Span::styled(text[mat.start()..mat.end()].to_string(), match_style));
        last = mat.end();
    }
    if last < text.len() {
        spans.push(Span::styled(text[last..].to_string(), base_style));
    }
    if spans.is_empty() {
        spans.push(Span::styled(text.to_string(), base_style));
    }
    Line::from(spans)
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() > max_chars {
        let t: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{t}…")
    } else {
        s.to_string()
    }
}

impl Widget for DataTable<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let cols = self.df.columns();

        // Inner area after block borders (1px each side).
        let inner_w = area.width.saturating_sub(2) as usize;
        let sp = COLUMN_SPACING;
        let max_col = ((inner_w as f32 * MAX_COL_FRAC) as usize).max(MIN_COL_WIDTH);

        let row_num_w = row_num_width(self.row_offset, self.df.height());
        let naturals: Vec<usize> = cols.iter().map(natural_col_width).collect();

        // ── Phase 1: greedily pick visible columns ────────────────────────
        // Row num slot = row_num_w (includes the │ char at end).
        // Each data column slot = sp + col_w.
        let mut vis_cols: Vec<usize> = Vec::new();
        let mut consumed = row_num_w;

        for (i, &nat) in naturals.iter().enumerate().skip(self.col_offset) {
            let w = nat.min(max_col);
            if consumed + sp + w > inner_w && !vis_cols.is_empty() {
                break;
            }
            vis_cols.push(i);
            consumed += sp + w;
        }

        if vis_cols.is_empty() {
            return;
        }

        // ── Phase 2: redistribute leftover space to capped columns ────────
        let capped: Vec<usize> = vis_cols.iter().map(|&i| naturals[i].min(max_col)).collect();
        let slack = inner_w.saturating_sub(consumed);
        let vis_naturals: Vec<usize> = vis_cols.iter().map(|&i| naturals[i]).collect();
        let final_widths = redistribute(capped, &vis_naturals, slack);

        // Actual pixels used by full columns (excluding the filler slot).
        let used: usize =
            row_num_w + final_widths.iter().map(|w| sp + w).sum::<usize>();

        // Remaining space for a partial right-edge column.
        let remaining = inner_w.saturating_sub(used);
        let next_col_idx = vis_cols.last().map(|&i| i + 1).unwrap_or(self.col_offset);
        let has_partial = remaining >= MIN_COL_WIDTH && next_col_idx < cols.len();
        let partial_width = if has_partial { remaining } else { 0 };

        // ── Build ratatui constraints ─────────────────────────────────────
        // Spacing baked into constraints → cursor bg fills the full row.
        // Layout: [row_num_w] [sp+col0] [sp+col1] ... [Min(0) filler]
        let mut widths: Vec<Constraint> = Vec::with_capacity(vis_cols.len() + 2);
        widths.push(Constraint::Length(row_num_w as u16));
        for &w in &final_widths {
            widths.push(Constraint::Length((sp + w) as u16));
        }
        widths.push(Constraint::Min(0));

        // ── Header ───────────────────────────────────────────────────────
        // UNDERLINED creates the horizontal separator line below the header.
        let hdr_style = Style::new()
            .bold()
            .fg(self.theme.header)
            .add_modifier(Modifier::UNDERLINED);
        let col_hdr_style = Style::new()
            .bold()
            .bg(self.theme.col_cursor_bg)
            .fg(self.theme.col_cursor_fg)
            .add_modifier(Modifier::UNDERLINED);

        // Row num header: right-align "#" with │ at the far right of the slot.
        let rn_hdr = format!("{:>w$}│", "#", w = row_num_w - 1);
        let mut header_cells = vec![Cell::new(rn_hdr).style(hdr_style)];

        for (idx, &ci) in vis_cols.iter().enumerate() {
            let name = truncate(cols[ci].name().as_str(), final_widths[idx]);
            let padded = format!("{:>width$}{}", "", name, width = sp);
            let style =
                if matches!(self.selection_mode, SelectionMode::Column | SelectionMode::Cell)
                    && ci == self.cursor_col
                {
                    col_hdr_style
                } else {
                    hdr_style
                };
            header_cells.push(Cell::new(padded).style(style));
        }

        // Partial column header — only add … if name doesn't fit.
        if has_partial && next_col_idx < cols.len() {
            let name = cols[next_col_idx].name().as_str();
            let display = if name.chars().count() >= partial_width {
                let clipped: String = name.chars().take(partial_width.saturating_sub(1)).collect();
                format!("{clipped}…")
            } else {
                name.to_string()
            };
            header_cells.push(Cell::new(display).style(hdr_style));
        } else {
            header_cells.push(Cell::new("").style(hdr_style));
        }
        let header = Row::new(header_cells);

        // ── Data rows ────────────────────────────────────────────────────
        let match_style = Style::new().bg(self.theme.match_bg).fg(self.theme.match_fg);
        let cursor_style = Style::new().bg(self.theme.cursor_bg).fg(self.theme.cursor_fg);
        let col_cursor_style =
            Style::new().bg(self.theme.col_cursor_bg).fg(self.theme.col_cursor_fg);

        // Absolute row index of the currently selected search match (if any).
        let current_match_row = self.search.and_then(|s| s.current_row());

        let rows: Vec<Row> = (0..self.df.height())
            .map(|ri| {
                let abs_row = self.row_offset + ri;
                let is_cursor = abs_row == self.cursor_row;
                let is_match_row = current_match_row == Some(abs_row);

                // Style for the row-number cell.
                let num_style = match self.selection_mode {
                    SelectionMode::Row if is_cursor => cursor_style,
                    SelectionMode::Column if is_match_row => cursor_style,
                    SelectionMode::Column if is_cursor => cursor_style,
                    _ => Style::new().fg(self.theme.row_num),
                };

                // Per-column style: depends on selection mode.
                //
                // Column mode layering (highest priority first):
                //   1. Current search-match row  → cursor_style (bright row bar)
                //   2. Cursor position row        → cursor_style (same, so j/k are visible)
                //   3. Selected column            → col_cursor_style
                //   4. Everything else            → default
                let cell_style = |ci: usize| -> Style {
                    match self.selection_mode {
                        SelectionMode::Row => {
                            if is_cursor { cursor_style } else { Style::default() }
                        }
                        SelectionMode::Column => {
                            if is_match_row || is_cursor {
                                cursor_style
                            } else if ci == self.cursor_col {
                                col_cursor_style
                            } else {
                                Style::default()
                            }
                        }
                        SelectionMode::Cell => {
                            if is_cursor && ci == self.cursor_col {
                                cursor_style
                            } else {
                                Style::default()
                            }
                        }
                    }
                };


                // Row number with │ vertical separator at the right edge of slot.
                let rn_str = format!("{:>w$}│", abs_row + 1, w = row_num_w - 1);
                let mut cells = vec![Cell::new(rn_str).style(num_style)];

                for (idx, &ci) in vis_cols.iter().enumerate() {
                    let val = match cols[ci].get(ri) {
                        Ok(v) => truncate(&format!("{v}"), final_widths[idx]),
                        Err(_) => "null".to_string(),
                    };
                    let cs = cell_style(ci);
                    // Scope highlights to the searched column when set.
                    let search = match self.search_col {
                        None => self.search,
                        Some(sc) => if ci == sc { self.search } else { None },
                    };

                    let cell = if let Some(s) = search {
                        let pad = Span::styled(format!("{:>width$}", "", width = sp), cs);
                        let mut spans = vec![pad];
                        spans.extend(highlight_cell(&val, s, cs, match_style).spans);
                        Cell::new(Line::from(spans)).style(cs)
                    } else {
                        Cell::new(format!("{:>width$}{}", "", val, width = sp)).style(cs)
                    };
                    cells.push(cell);
                }

                // Partial right-edge column — only truncate+ellipsis when needed.
                if has_partial && next_col_idx < cols.len() {
                    let val = match cols[next_col_idx].get(ri) {
                        Ok(v) => format!("{v}"),
                        Err(_) => "null".to_string(),
                    };
                    let display = if val.chars().count() >= partial_width {
                        let clipped: String =
                            val.chars().take(partial_width.saturating_sub(1)).collect();
                        format!("{clipped}…")
                    } else {
                        val
                    };
                    let cs = cell_style(next_col_idx);
                    let search = match self.search_col {
                        None => self.search,
                        Some(sc) => if next_col_idx == sc { self.search } else { None },
                    };
                    let cell = if let Some(s) = search {
                        Cell::new(highlight_cell(&display, s, cs, match_style)).style(cs)
                    } else {
                        Cell::new(display).style(cs)
                    };
                    cells.push(cell);
                } else {
                    cells.push(Cell::new("").style(Style::default()));
                }

                Row::new(cells)
            })
            .collect();

        // Tell the app layer which column is the rightmost fully visible one.
        self.last_vis_col_out
            .set(vis_cols.last().copied().unwrap_or(self.col_offset));

        // ── Render ───────────────────────────────────────────────────────
        let border_style = Style::new().fg(self.theme.border);
        let block = Block::new()
            .borders(Borders::ALL)
            .border_style(border_style);

        Table::new(rows, widths)
            .header(header)
            .block(block)
            .column_spacing(0)
            .render(area, buf);
    }
}
