use polars::prelude::*;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Rect},
    style::{Color, Style},
    widgets::{Block, Borders, Cell, Row, Table, Widget},
};

const MIN_COL_WIDTH: usize = 3;
/// Columns are separated by this many spaces (matches csvlens' NUM_SPACES_BETWEEN_COLUMNS).
const COLUMN_SPACING: u16 = 4;
const ROW_NUM_WIDTH: u16 = 6;
/// A column may consume at most this fraction of the available terminal width.
const MAX_COL_FRAC: f32 = 0.3;

pub struct DataTable<'a> {
    pub df: &'a DataFrame,
    pub col_offset: usize,
    pub row_offset: usize,
    pub cursor_row: usize,
    pub title: &'a str,
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
fn redistribute(
    mut widths: Vec<usize>,
    naturals: &[usize],
    slack: usize,
) -> Vec<usize> {
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

        // Inner area after block borders (ALL = 1px each side).
        let inner_w = area.width.saturating_sub(2) as usize;
        let row_num_w = ROW_NUM_WIDTH as usize;
        let sp = COLUMN_SPACING as usize;
        let max_col = ((inner_w as f32 * MAX_COL_FRAC) as usize).max(MIN_COL_WIDTH);

        let naturals: Vec<usize> = cols.iter().map(natural_col_width).collect();

        // ── Phase 1: greedily pick visible columns ────────────────────────
        // Accounting: consumed = row_num_w + Σ(sp + col_w) for each included col.
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

        // ── Build ratatui constraints ─────────────────────────────────────
        let mut widths: Vec<Constraint> = Vec::with_capacity(vis_cols.len() + 1);
        widths.push(Constraint::Length(ROW_NUM_WIDTH));
        for &w in &final_widths {
            widths.push(Constraint::Length(w as u16));
        }

        // ── Header ───────────────────────────────────────────────────────
        let hdr_style = Style::new().bold().fg(Color::Rgb(131, 148, 150));
        let mut header_cells = vec![Cell::new("#").style(hdr_style)];
        for (idx, &ci) in vis_cols.iter().enumerate() {
            let name = truncate(cols[ci].name().as_str(), final_widths[idx]);
            header_cells.push(Cell::new(name).style(hdr_style));
        }
        let header = Row::new(header_cells);

        // ── Data rows ────────────────────────────────────────────────────
        let rows: Vec<Row> = (0..self.df.height())
            .map(|ri| {
                let abs_row = self.row_offset + ri;
                let is_cursor = abs_row == self.cursor_row;

                let row_style = if is_cursor {
                    Style::new()
                        .bg(Color::Rgb(62, 61, 50))
                        .fg(Color::Rgb(192, 192, 192))
                } else {
                    Style::default()
                };

                let num_style = if is_cursor {
                    row_style
                } else {
                    Style::new().fg(Color::Rgb(131, 148, 150))
                };

                let mut cells =
                    vec![Cell::new((abs_row + 1).to_string()).style(num_style)];

                for (idx, &ci) in vis_cols.iter().enumerate() {
                    let val = match cols[ci].get(ri) {
                        Ok(v) => truncate(&format!("{v}"), final_widths[idx]),
                        Err(_) => "null".to_string(),
                    };
                    cells.push(Cell::new(val).style(row_style));
                }

                Row::new(cells)
            })
            .collect();

        // ── Render ───────────────────────────────────────────────────────
        let border_style = Style::new().fg(Color::Rgb(80, 80, 95));
        let title_style = Style::new().fg(Color::Rgb(131, 148, 150));
        let block = Block::new()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(format!(" {} ", self.title))
            .title_style(title_style);

        Table::new(rows, widths)
            .header(header)
            .block(block)
            .column_spacing(COLUMN_SPACING)
            .render(area, buf);
    }
}
