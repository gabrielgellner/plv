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
    /// Active sort keys in priority order: `(column_index, ascending)`. Empty = unsorted.
    pub sort: &'a [(usize, bool)],
    /// `Some(tick)` while a background sort is in progress; drives the header animation.
    pub sort_tick: Option<usize>,
    /// Cells with an unwritten edit, as `(row within the page, column index)`.
    pub edited: &'a [(usize, usize)],
    /// The visual selection, as absolute inclusive `(row range, column range)`.
    pub selection: Option<((usize, usize), (usize, usize))>,
    /// Number rows by their distance from the cursor rather than absolutely.
    pub relative_rows: bool,
    /// Widths set by hand, by source column index.
    pub widths: &'a Widths,
}

/// The row-number cell: each row's distance from the cursor, and on the cursor
/// line the row's own number.
///
/// This is nvim's hybrid `number` + `relativenumber` gutter, and the alignment
/// is the point of it. The current line is left-aligned where the distances are
/// right-aligned, so it reads as outdented against the column beside it — and
/// both branches fill the same width, so nothing shifts as the cursor moves.
///
/// `width` is the space before the `│`, which closes the column.
fn gutter(abs_row: usize, cursor_row: usize, relative: bool, width: usize) -> String {
    let body = if !relative {
        format!("{:>width$}", abs_row + 1)
    } else if abs_row == cursor_row {
        format!("{:<width$}", abs_row + 1)
    } else {
        format!("{:>width$}", abs_row.abs_diff(cursor_row))
    };
    format!("{body}│")
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

/// What an empty cell shows.
///
/// An absent field and an empty one are the same thing to plv — both read as
/// null and both are written back as an empty field — so one marker covers
/// both. A marker rather than nothing at all, because a genuinely blank cell
/// is indistinguishable from a column that simply ends there; and a marker
/// rather than the word `null`, which reads as data and collides with a field
/// whose text really is "null".
const EMPTY: &str = "\u{b7}";

/// One cell as the viewer shows it.
///
/// `AnyValue`'s own `Display` wraps strings in quotes — it is written for
/// debugging, where telling `1` from `"1"` matters. In a viewer over a file
/// that is text to begin with, the quotes are noise that also costs two
/// columns of width per cell. `str_value` gives the bare text for strings and
/// categoricals, and `Display` for everything else, so numbers, booleans and
/// dates are unchanged. Nulls become [`EMPTY`] rather than the word `null`.
fn cell_text(value: AnyValue) -> String {
    match value {
        AnyValue::Null => EMPTY.to_string(),
        value => value.str_value().into_owned(),
    }
}

fn natural_col_width(col: &Column) -> usize {
    // Counted in characters, because that is what `truncate` and the column
    // layout work in — bytes would over-size any column holding non-ASCII.
    let header_w = col.name().chars().count();
    let data_w = (0..col.len())
        .map(|i| {
            col.get(i)
                .map(|v| cell_text(v).chars().count())
                .unwrap_or(0)
        })
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
fn highlight_cell(
    text: &str,
    state: &SearchState,
    base_style: Style,
    match_style: Style,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut last = 0;
    for mat in state.query.regex.find_iter(text) {
        if mat.start() > last {
            spans.push(Span::styled(
                text[last..mat.start()].to_string(),
                base_style,
            ));
        }
        spans.push(Span::styled(
            text[mat.start()..mat.end()].to_string(),
            match_style,
        ));
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

/// The `col_offset` that puts `target` at the right edge: the leftmost start
/// that still shows `target` in full.
///
/// Lives beside the renderer that has to agree with it. The app layer used to
/// keep its own copy of this arithmetic, and the two drifted — the copy was
/// still measuring quoted strings in bytes after the renderer had stopped.
/// Column widths the user has set by hand, by source column index.
///
/// A set width is used as given — the cap that keeps one column from taking
/// the whole screen is a default, not a rule, and overriding it is the point.
/// Columns after a widened one are pushed along and off the right edge, as a
/// spreadsheet does, rather than everything shuffling to make room.
pub type Widths = std::collections::HashMap<usize, usize>;

/// The width to draw a column at: what was set for it, or what it needs.
fn width_of(column: &Column, source: usize, set: &Widths, cap: usize) -> usize {
    match set.get(&source) {
        Some(&width) => width.max(MIN_COL_WIDTH),
        None => natural_col_width(column).min(cap),
    }
}

/// The narrowest a column may be made.
pub const MIN_COLUMN: usize = MIN_COL_WIDTH;

/// What a column needs to show every value on the current page in full.
pub fn natural_width(column: &Column) -> usize {
    natural_col_width(column)
}

/// What a column is drawn at right now, so an adjustment starts from what is
/// on screen rather than from nothing.
///
/// Asked of the layout rather than worked out again by the caller: the app
/// layer once kept its own copy of this arithmetic and the two drifted.
pub fn drawn_width(column: &Column, source: usize, frame_width: u16, set: &Widths) -> usize {
    let inner = (frame_width as usize).saturating_sub(2);
    let cap = ((inner as f32 * MAX_COL_FRAC) as usize).max(MIN_COL_WIDTH);
    width_of(column, source, set, cap)
}

pub fn col_offset_showing(
    df: &DataFrame,
    row_offset: usize,
    frame_width: u16,
    target: usize,
    widths: &Widths,
) -> usize {
    let cols = df.columns();
    if cols.is_empty() {
        return 0;
    }
    let target = target.min(cols.len() - 1);

    let inner_w = (frame_width as usize).saturating_sub(2);
    let max_col = ((inner_w as f32 * MAX_COL_FRAC) as usize).max(MIN_COL_WIDTH);
    let row_num_w = row_num_width(row_offset, df.height());
    let slot = |ci: usize| COLUMN_SPACING + width_of(&cols[ci], ci, widths, max_col);

    let Some(mut budget) = inner_w.saturating_sub(row_num_w).checked_sub(slot(target)) else {
        return target; // the target alone does not fit — show it at the left edge
    };

    // Walk left from the target, taking every column that still fits.
    let mut offset = target;
    for ci in (0..target).rev() {
        if budget < slot(ci) {
            break;
        }
        budget -= slot(ci);
        offset = ci;
    }
    offset
}

impl Widget for DataTable<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let cols = self.df.columns();

        // Inner area after block borders (1px each side).
        let inner_w = area.width.saturating_sub(2) as usize;
        let sp = COLUMN_SPACING;
        let max_col = ((inner_w as f32 * MAX_COL_FRAC) as usize).max(MIN_COL_WIDTH);

        let row_num_w = row_num_width(self.row_offset, self.df.height());
        // What each column asks for: a width set by hand is what it asks for
        // and what it gets, exempt from the cap below.
        let naturals: Vec<usize> = cols
            .iter()
            .enumerate()
            .map(|(index, column)| match self.widths.get(&index) {
                Some(&width) => width.max(MIN_COL_WIDTH),
                None => natural_col_width(column),
            })
            .collect();

        // ── Phase 1: greedily pick visible columns ────────────────────────
        // Row num slot = row_num_w (includes the │ char at end).
        // Each data column slot = sp + col_w.
        let mut vis_cols: Vec<usize> = Vec::new();
        let mut consumed = row_num_w;

        for (i, &nat) in naturals.iter().enumerate().skip(self.col_offset) {
            let w = if self.widths.contains_key(&i) {
                nat
            } else {
                nat.min(max_col)
            };
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
        let capped: Vec<usize> = vis_cols
            .iter()
            .map(|&i| {
                if self.widths.contains_key(&i) {
                    naturals[i]
                } else {
                    naturals[i].min(max_col)
                }
            })
            .collect();
        let slack = inner_w.saturating_sub(consumed);
        let vis_naturals: Vec<usize> = vis_cols.iter().map(|&i| naturals[i]).collect();
        let final_widths = redistribute(capped, &vis_naturals, slack);

        // Actual pixels used by full columns (excluding the filler slot).
        let used: usize = row_num_w + final_widths.iter().map(|w| sp + w).sum::<usize>();

        // Remaining space for a partial right-edge column.
        let remaining = inner_w.saturating_sub(used);
        let next_col_idx = vis_cols.last().map(|&i| i + 1).unwrap_or(self.col_offset);
        // The partial column carries the same lead-in as a full one. Without
        // it the last full column's header runs straight into this one's and
        // the two read as a single strange name.
        let has_partial = remaining >= sp + MIN_COL_WIDTH && next_col_idx < cols.len();
        let partial_width = if has_partial { remaining - sp } else { 0 };

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
            let is_cursor_col = matches!(
                self.selection_mode,
                SelectionMode::Column | SelectionMode::Cell
            ) && ci == self.cursor_col;
            let cell_style = if is_cursor_col {
                col_hdr_style
            } else {
                hdr_style
            };
            let fg = if is_cursor_col {
                self.theme.col_cursor_fg
            } else {
                self.theme.header
            };

            let sort_entry = self.sort.iter().enumerate().find(|(_, key)| key.0 == ci);

            let cell = if let (Some(tick), Some((order, key))) = (self.sort_tick, sort_entry) {
                // Sort in progress: animate [ arrow num ] with cycling bold.
                let arrow = if key.1 { "▲" } else { "▼" };
                let num = (order + 1).to_string();
                // indicator is " [▲N]" — 4 chars for "[▲N]" plus 1 space = 5 + num digits
                let indicator_len = 1 + 1 + 1 + num.len() + 1;
                let avail = final_widths[idx].saturating_sub(indicator_len);
                let name = truncate(cols[ci].name().as_str(), avail);

                let bold_pos = (tick / 3) % 3; // 0="[", 1=arrow, 2="]"
                let active = Style::new()
                    .fg(fg)
                    .bold()
                    .add_modifier(Modifier::UNDERLINED);
                let quiet = Style::new()
                    .fg(fg)
                    .add_modifier(Modifier::UNDERLINED)
                    .remove_modifier(Modifier::BOLD);

                let spans = vec![
                    Span::styled(format!("{:>width$}{}", "", name, width = sp), cell_style),
                    Span::styled(" ", quiet),
                    Span::styled("[", if bold_pos == 0 { active } else { quiet }),
                    Span::styled(arrow, if bold_pos == 1 { active } else { quiet }),
                    Span::styled(num, quiet),
                    Span::styled("]", if bold_pos == 2 { active } else { quiet }),
                ];
                Cell::new(Line::from(spans)).style(cell_style)
            } else {
                // Static: show completed sort indicator.
                let sort_indicator = sort_entry
                    .map(|(order, key)| {
                        let arrow = if key.1 { "▲" } else { "▼" };
                        format!(" [{arrow}{}]", order + 1)
                    })
                    .unwrap_or_default();
                let avail = final_widths[idx].saturating_sub(sort_indicator.chars().count());
                let name = truncate(cols[ci].name().as_str(), avail);
                let padded = format!("{:>width$}{}{}", "", name, sort_indicator, width = sp);
                Cell::new(padded).style(cell_style)
            };
            header_cells.push(cell);
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
            header_cells.push(Cell::new(format!("{:>sp$}{display}", "")).style(hdr_style));
        } else {
            header_cells.push(Cell::new("").style(hdr_style));
        }
        let header = Row::new(header_cells);

        // ── Data rows ────────────────────────────────────────────────────
        let match_style = Style::new().bg(self.theme.match_bg).fg(self.theme.match_fg);
        let cursor_style = Style::new()
            .bg(self.theme.cursor_bg)
            .fg(self.theme.cursor_fg);
        let col_cursor_style = Style::new()
            .bg(self.theme.col_cursor_bg)
            .fg(self.theme.col_cursor_fg);
        let selection_style = Style::new()
            .bg(self.theme.selection_bg)
            .fg(self.theme.selection_fg);

        // Absolute row index of the currently selected search match (if any).
        let current_match_row = self.search.and_then(|s| s.current_row());

        let rows: Vec<Row> = (0..self.df.height())
            .map(|ri| {
                let abs_row = self.row_offset + ri;
                let is_cursor = abs_row == self.cursor_row;
                let is_match_row = current_match_row == Some(abs_row);

                // Style for the row-number cell. The gutter sits well back
                // from the data, so the current line needs its own colour to
                // stay findable among the distances.
                let num_style = match self.selection_mode {
                    SelectionMode::Row if is_cursor => cursor_style,
                    SelectionMode::Column if is_match_row => cursor_style,
                    SelectionMode::Column if is_cursor => cursor_style,
                    _ if is_cursor => Style::new().fg(self.theme.row_num_cursor),
                    _ => Style::new().fg(self.theme.row_num),
                };

                // Per-column style: depends on selection mode.
                //
                // Column mode layering (highest priority first):
                //   1. Current search-match row  → cursor_style (bright row bar)
                //   2. Cursor position row        → cursor_style (same, so j/k are visible)
                //   3. Selected column            → col_cursor_style
                //   4. Everything else            → default
                // A visual selection sits *under* the cursor highlights, so
                // the cursor stays findable inside its own selection.
                let unselected = |ci: usize| -> Style {
                    let inside = self.selection.is_some_and(|((r0, r1), (c0, c1))| {
                        (r0..=r1).contains(&abs_row) && (c0..=c1).contains(&ci)
                    });
                    if inside {
                        selection_style
                    } else {
                        Style::default()
                    }
                };

                let cell_style = |ci: usize| -> Style {
                    match self.selection_mode {
                        SelectionMode::Row => {
                            if is_cursor {
                                cursor_style
                            } else {
                                unselected(ci)
                            }
                        }
                        SelectionMode::Column => {
                            if is_match_row || is_cursor {
                                cursor_style
                            } else if ci == self.cursor_col {
                                col_cursor_style
                            } else {
                                unselected(ci)
                            }
                        }
                        SelectionMode::Cell => {
                            if is_cursor && ci == self.cursor_col {
                                cursor_style
                            } else {
                                unselected(ci)
                            }
                        }
                    }
                };

                let rn_str = gutter(abs_row, self.cursor_row, self.relative_rows, row_num_w - 1);
                let mut cells = vec![Cell::new(rn_str).style(num_style)];

                for (idx, &ci) in vis_cols.iter().enumerate() {
                    let value = cols[ci].get(ri);
                    let is_empty = !matches!(&value, Ok(v) if !v.is_null());
                    let val = match value {
                        Ok(v) => truncate(&cell_text(v), final_widths[idx]),
                        Err(_) => EMPTY.to_string(),
                    };
                    let cs = cell_style(ci);
                    // An empty cell recedes, so a column of them reads as a
                    // gap rather than as content.
                    let cs = if is_empty {
                        cs.fg(self.theme.null_fg)
                    } else {
                        cs
                    };
                    // Call out an unwritten edit, so the state of the buffer is
                    // visible in the grid and not only as a count in the status
                    // bar. Foreground only, so it layers over the cursor and the
                    // alternating row backgrounds rather than replacing them.
                    let cs = if self.edited.contains(&(ri, ci)) {
                        cs.fg(self.theme.edited_fg).add_modifier(Modifier::BOLD)
                    } else {
                        cs
                    };
                    // Scope highlights to the searched column when set.
                    let search = match self.search_col {
                        None => self.search,
                        Some(sc) => {
                            if ci == sc {
                                self.search
                            } else {
                                None
                            }
                        }
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
                        Ok(v) => cell_text(v),
                        Err(_) => EMPTY.to_string(),
                    };
                    let display = if val.chars().count() >= partial_width {
                        let clipped: String =
                            val.chars().take(partial_width.saturating_sub(1)).collect();
                        format!("{clipped}…")
                    } else {
                        val
                    };
                    let display = format!("{:>sp$}{display}", "");
                    let cs = cell_style(next_col_idx);
                    let search = match self.search_col {
                        None => self.search,
                        Some(sc) => {
                            if next_col_idx == sc {
                                self.search
                            } else {
                                None
                            }
                        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gutter_counts_from_the_cursor() {
        // Distances above and below, and the row's own number on the line
        // itself. Width 3, so each cell is 4 characters with the separator.
        assert_eq!(gutter(0, 2, true, 3), "  2│");
        assert_eq!(gutter(1, 2, true, 3), "  1│");
        assert_eq!(gutter(2, 2, true, 3), "3  │");
        assert_eq!(gutter(3, 2, true, 3), "  1│");
        assert_eq!(gutter(9, 2, true, 3), "  7│");
    }

    #[test]
    fn the_current_line_is_outdented_against_the_distances() {
        // The property that has to hold: both branches fill the same width, so
        // the column does not shift as the cursor moves, and the current line
        // reads as outdented because it alone is left-aligned.
        let cursor = gutter(41, 41, true, 4);
        let neighbour = gutter(42, 41, true, 4);
        assert_eq!(cursor, "42  │");
        assert_eq!(neighbour, "   1│");
        assert_eq!(cursor.chars().count(), neighbour.chars().count());
    }

    #[test]
    fn absolute_numbering_right_aligns_every_row() {
        assert_eq!(gutter(0, 2, false, 3), "  1│");
        assert_eq!(gutter(2, 2, false, 3), "  3│");
        assert_eq!(gutter(41, 2, false, 3), " 42│");
    }

    #[test]
    fn a_number_wider_than_the_gutter_still_closes_the_column() {
        // Numbers overflow their field rather than being truncated: a wrong
        // number would be worse than a wide one.
        assert!(gutter(5000, 0, true, 3).ends_with('│'));
        assert!(gutter(5000, 0, false, 3).ends_with('│'));
    }

    /// Every rendered line, so what is checked is what reaches the screen
    /// rather than what the function meant to put there.
    fn rendered(cursor_row: usize, relative: bool) -> Vec<String> {
        let df = df! {
            "name" => ["a", "b", "c", "d", "e"],
            "n" => [1, 2, 3, 4, 5],
        }
        .unwrap();
        lines(&draw(&df, cursor_row, relative))
    }

    fn lines(buf: &Buffer) -> Vec<String> {
        (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    fn draw(df: &DataFrame, cursor_row: usize, relative: bool) -> Buffer {
        let theme = Theme::catppuccin_mocha();
        let last_vis = std::cell::Cell::new(0);
        let area = Rect::new(0, 0, 40, 10);
        let mut buf = Buffer::empty(area);

        DataTable {
            df,
            col_offset: 0,
            cursor_col: 0,
            row_offset: 0,
            cursor_row,
            selection_mode: SelectionMode::Cell,
            theme: &theme,
            search: None,
            search_col: None,
            last_vis_col_out: &last_vis,
            sort: &[],
            sort_tick: None,
            edited: &[],
            selection: None,
            relative_rows: relative,
            widths: &Widths::new(),
        }
        .render(area, &mut buf);
        buf
    }

    #[test]
    fn the_rendered_gutter_counts_from_the_cursor_row() {
        let lines = rendered(2, true);
        let gutters: Vec<String> = lines
            .iter()
            // Field 0 is before the block's own left border, so the gutter
            // is the next one along.
            .filter_map(|line| line.split('\u{2502}').nth(1).map(|g| g.trim().to_string()))
            .filter(|gutter| !gutter.is_empty()) // blank rows below the data
            .collect();

        // The header's "#", then the five data rows around a cursor on row 2.
        assert_eq!(gutters, ["#", "2", "1", "3", "1", "2"], "{lines:#?}");
    }

    #[test]
    fn the_rendered_gutter_can_be_switched_to_absolute() {
        let lines = rendered(2, false);
        let gutters: Vec<String> = lines
            .iter()
            // Field 0 is before the block's own left border, so the gutter
            // is the next one along.
            .filter_map(|line| line.split('\u{2502}').nth(1).map(|g| g.trim().to_string()))
            .filter(|gutter| !gutter.is_empty()) // blank rows below the data
            .collect();

        assert_eq!(gutters, ["#", "1", "2", "3", "4", "5"], "{lines:#?}");
    }

    #[test]
    fn text_cells_are_shown_without_the_debug_quoting() {
        let lines = rendered(0, true);
        let body = lines.join("\n");
        assert!(
            body.contains(" a ") && !body.contains("\"a\""),
            "strings should render bare:\n{body}"
        );
    }

    #[test]
    fn cell_text_quotes_nothing_and_still_names_a_null() {
        assert_eq!(cell_text(AnyValue::String("alpha")), "alpha");
        assert_eq!(cell_text(AnyValue::StringOwned("beta".into())), "beta");
        // A string that looks like a number is still shown as it reads in the
        // file; the column header and its alignment say what the type is.
        assert_eq!(cell_text(AnyValue::String("1")), "1");
        assert_eq!(cell_text(AnyValue::Int64(42)), "42");
        assert_eq!(cell_text(AnyValue::Float64(1.5)), "1.5");
        assert_eq!(cell_text(AnyValue::Boolean(true)), "true");
        assert_eq!(cell_text(AnyValue::Null), EMPTY);
        // A field whose text really is "null" is no longer confusable with
        // one that holds nothing.
        assert_ne!(
            cell_text(AnyValue::String("null")),
            cell_text(AnyValue::Null)
        );
    }

    #[test]
    fn a_column_is_measured_in_characters_not_bytes() {
        // Bytes would make this column three wider than it renders.
        let df = df! { "n" => ["éàü"] }.unwrap();
        assert_eq!(natural_col_width(&df.columns()[0]), MIN_COL_WIDTH.max(3));
    }

    #[test]
    fn quoting_no_longer_pads_the_column_width() {
        let df = df! { "s" => ["alpha"] }.unwrap();
        // "alpha" is five characters; the quotes used to make it seven.
        assert_eq!(natural_col_width(&df.columns()[0]), 5);
    }

    fn draw_narrow(df: &DataFrame, width: u16, col_offset: usize) -> Buffer {
        let theme = Theme::catppuccin_mocha();
        let last_vis = std::cell::Cell::new(0);
        let area = Rect::new(0, 0, width, 4);
        let mut buf = Buffer::empty(area);
        DataTable {
            df,
            col_offset,
            cursor_col: col_offset,
            row_offset: 0,
            cursor_row: 0,
            selection_mode: SelectionMode::Cell,
            theme: &theme,
            search: None,
            search_col: None,
            last_vis_col_out: &last_vis,
            sort: &[],
            sort_tick: None,
            edited: &[],
            selection: None,
            relative_rows: true,
            widths: &Widths::new(),
        }
        .render(area, &mut buf);
        buf
    }

    /// A column that only partly fits still has to start clear of the one
    /// before it, or the two headers run together and read as one odd name.
    #[test]
    fn a_partial_column_keeps_its_distance_from_the_last_full_one() {
        let names = ["verdict", "shared", "our_tracks", "their_tracks", "matched"];
        let df = df! {
            "verdict" => ["weak"],
            "shared" => ["0.733"],
            "our_tracks" => ["15"],
            "their_tracks" => ["11"],
            "matched" => ["Adele"],
        }
        .unwrap();

        // Sweep the widths where the last column is cut off part way.
        for width in 40..70 {
            let rendered = lines(&draw_narrow(&df, width, 0))[1].clone();
            // Field 0 is the block border and field 1 the gutter; the headers
            // are what follows.
            let header = rendered.split('\u{2502}').nth(2).unwrap_or("").to_string();
            for token in header.split_whitespace() {
                let bare = token.trim_end_matches('\u{2026}');
                assert!(
                    names.iter().any(|name| name.starts_with(bare)),
                    "at width {width}, {token:?} is not a column name: {rendered:?}"
                );
            }
        }
    }

    #[test]
    fn an_empty_cell_is_drawn_recessive() {
        // Read the colour off the buffer: a style the code passes but the
        // screen does not show is not a colour.
        let df = df! {
            "name" => [Some("a"), None],
            "n" => [1, 2],
        }
        .unwrap();
        let buf = draw(&df, 0, true);
        let theme = Theme::catppuccin_mocha();

        let marker = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .find(|&(x, y)| buf[(x, y)].symbol() == EMPTY)
            .expect("the empty cell should be marked");

        assert_eq!(buf[marker].fg, theme.null_fg);
        assert_ne!(theme.null_fg, theme.cursor_fg, "and not the data colour");
    }

    #[test]
    fn an_empty_column_does_not_reserve_room_for_the_word_null() {
        let df = df! { "s" => [None::<&str>, None] }.unwrap();
        assert_eq!(natural_col_width(&df.columns()[0]), MIN_COL_WIDTH);
    }

    /// The width the gutter asks for only grows at powers of ten, so scrolling
    /// does not make the whole table shuffle sideways.
    #[test]
    fn the_gutter_width_is_stable_between_orders_of_magnitude() {
        assert_eq!(row_num_width(0, 20), row_num_width(30, 20));
        assert!(row_num_width(0, 20) < row_num_width(100_000, 20));
    }
}
