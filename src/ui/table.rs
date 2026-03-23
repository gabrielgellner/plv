use polars::prelude::*;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Rect},
    style::{Color, Style},
    widgets::{Block, Borders, Cell, Row, Table, Widget},
};

const MAX_COL_WIDTH: usize = 20;
const MIN_COL_WIDTH: usize = 4;
const ROW_NUM_WIDTH: u16 = 6;

pub struct DataTable<'a> {
    pub df: &'a DataFrame,
    pub col_offset: usize,
    pub row_offset: usize,
    pub cursor_row: usize,
    pub title: &'a str,
}

fn col_display_width(col: &Column) -> u16 {
    let header_w = col.name().len();
    let data_w = (0..col.len())
        .map(|i| col.get(i).map(|v| format!("{v}").len()).unwrap_or(4))
        .max()
        .unwrap_or(0);
    header_w.max(data_w).clamp(MIN_COL_WIDTH, MAX_COL_WIDTH) as u16
}

impl Widget for DataTable<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let cols = self.df.columns();

        let all_widths: Vec<u16> = cols.iter().map(col_display_width).collect();

        // Inner width = area - 2 (borders). Row-num col + separator consume ROW_NUM_WIDTH + 1.
        let available = area.width.saturating_sub(2 + ROW_NUM_WIDTH + 1) as usize;
        let mut vis_cols: Vec<usize> = Vec::new();
        let mut used = 0usize;
        for (i, &w) in all_widths.iter().enumerate().skip(self.col_offset) {
            let needed = w as usize + 1; // +1 for column separator
            if !vis_cols.is_empty() && used + needed > available {
                break;
            }
            vis_cols.push(i);
            used += needed;
        }

        let mut widths: Vec<Constraint> = Vec::with_capacity(vis_cols.len() + 1);
        widths.push(Constraint::Length(ROW_NUM_WIDTH));
        for &ci in &vis_cols {
            widths.push(Constraint::Length(all_widths[ci]));
        }

        // Header
        let mut header_cells = vec![Cell::new("#").style(Style::new().bold().fg(Color::Yellow))];
        for &ci in &vis_cols {
            header_cells.push(
                Cell::new(cols[ci].name().to_string()).style(Style::new().bold().fg(Color::Cyan)),
            );
        }
        let header = Row::new(header_cells);

        // Data rows
        let rows: Vec<Row> = (0..self.df.height())
            .map(|ri| {
                let abs_row = self.row_offset + ri;
                let is_cursor = abs_row == self.cursor_row;

                let row_style = if is_cursor {
                    Style::new().bg(Color::Rgb(60, 90, 150))
                } else if ri % 2 == 1 {
                    Style::new().bg(Color::Rgb(30, 30, 30))
                } else {
                    Style::default()
                };

                let num_style = if is_cursor {
                    row_style.fg(Color::White)
                } else {
                    Style::new().fg(Color::DarkGray)
                };

                let mut cells =
                    vec![Cell::new((abs_row + 1).to_string()).style(num_style)];

                for &ci in &vis_cols {
                    let val = match cols[ci].get(ri) {
                        Ok(v) => {
                            let s = format!("{v}");
                            if s.chars().count() > MAX_COL_WIDTH {
                                let t: String = s.chars().take(MAX_COL_WIDTH - 1).collect();
                                format!("{t}…")
                            } else {
                                s
                            }
                        }
                        Err(_) => "null".to_string(),
                    };
                    cells.push(Cell::new(val).style(row_style));
                }

                Row::new(cells)
            })
            .collect();

        let block = Block::new()
            .borders(Borders::ALL)
            .title(format!(" {} ", self.title));

        Table::new(rows, widths).header(header).block(block).render(area, buf);
    }
}
