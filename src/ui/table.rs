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
/// The │ that closes the pinned block, one character wide.
const PIN_DIVIDER: usize = 1;

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
    /// Columns held at the left edge, as **display** indices.
    pub pinned: &'a Pinned,
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

/// The sort indicator in full and in brief.
///
/// The number is the key's priority and says nothing at all when there is only
/// one key; the arrow is the part that cannot be dropped, since it is what
/// tells you which way the column is sorted.
fn sort_marks(order: usize, ascending: bool) -> (String, String) {
    let arrow = if ascending { "▲" } else { "▼" };
    (format!(" [{arrow}{}]", order + 1), arrow.to_string())
}

/// How much room beyond its name a column's header would like: the sort
/// indicator, when it has one.
///
/// Counted into what a column *asks* for and not into what it is granted, so
/// that slack reaches a sorted column ahead of an unsorted one without
/// changing which columns are visible. Growing the visible set when a sort
/// starts would shift the table sideways under the user, and would disagree
/// with `col_offset_showing`, which cannot see the sort.
fn indicator_width(sort: &[(usize, bool)], ci: usize) -> usize {
    sort.iter()
        .position(|key| key.0 == ci)
        .map(|order| sort_marks(order, true).0.chars().count())
        .unwrap_or(0)
}

/// A header as it will be drawn: the name, and whatever of the indicator fits
/// beside it.
///
/// The name is what identifies the column, so it is the last thing to be given
/// up — a six-wide `region` used to render as a bare `…`, all six characters
/// spent on ` [▲1]`, which named the sort and lost the column. The indicator
/// gives way first, to its arrow and then to nothing.
fn header_parts(name: &str, width: usize, marks: Option<(String, String)>) -> (String, String) {
    let Some((full, brief)) = marks else {
        return (truncate(name, width), String::new());
    };
    let name_w = name.chars().count();
    if name_w + full.chars().count() <= width {
        (name.to_string(), full)
    } else if name_w + brief.chars().count() <= width {
        (name.to_string(), brief)
    } else {
        let brief_w = brief.chars().count();
        (truncate(name, width.saturating_sub(brief_w)), brief)
    }
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

/// Columns held at the left edge while the rest scroll past them, as display
/// indices.
///
/// A set and not a count: what is pinned is the columns the user picked out,
/// which need not be a prefix of the table — the point of pinning an id beside
/// a value is to read two columns that are far apart together.
pub type Pinned = std::collections::BTreeSet<usize>;

/// The pinned columns this frame actually has, in order.
///
/// A pin outlives the view it was made in: a `:select` can narrow the table to
/// fewer columns than there were when the pin was set, and the pin comes back
/// when the column does.
fn pin_cols(cols: &[Column], pinned: &Pinned) -> Vec<usize> {
    pinned
        .iter()
        .copied()
        .filter(|&ci| ci < cols.len())
        .collect()
}

/// The width the pinned block takes at the left edge: each pinned column's
/// slot, plus the divider that closes it.
///
/// Zero when nothing is pinned — there is no divider to draw and no space to
/// take — so an unpinned table lays out exactly as it did before.
fn pin_reserve(cols: &[Column], pins: &[usize], widths: &Widths, max_col: usize) -> usize {
    if pins.is_empty() {
        return 0;
    }
    pins.iter()
        .map(|&ci| COLUMN_SPACING + width_of(&cols[ci], ci, widths, max_col))
        .sum::<usize>()
        + PIN_DIVIDER
}

/// Whether a pinned block would still leave room to scroll in.
///
/// Pin enough of a wide table and the scrolling region disappears: the view
/// stops answering `h` and `l`, with nothing on screen to say why. plv refuses
/// the pin instead — the same call `sort_blocked` makes, for the same reason.
/// A refusal that names itself beats a view that quietly stops working.
pub fn pin_fits(
    df: &DataFrame,
    row_offset: usize,
    frame_width: u16,
    widths: &Widths,
    pinned: &Pinned,
) -> bool {
    let cols = df.columns();
    let inner_w = (frame_width as usize).saturating_sub(2);
    let max_col = ((inner_w as f32 * MAX_COL_FRAC) as usize).max(MIN_COL_WIDTH);
    let row_num_w = row_num_width(row_offset, df.height());
    let pins = pin_cols(cols, pinned);
    let reserve = pin_reserve(cols, &pins, widths, max_col);
    row_num_w + reserve + COLUMN_SPACING + MIN_COL_WIDTH <= inner_w
}

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
    pinned: &Pinned,
) -> usize {
    let cols = df.columns();
    if cols.is_empty() {
        return 0;
    }
    let target = target.min(cols.len() - 1);

    let inner_w = (frame_width as usize).saturating_sub(2);
    let max_col = ((inner_w as f32 * MAX_COL_FRAC) as usize).max(MIN_COL_WIDTH);
    let row_num_w = row_num_width(row_offset, df.height());
    let pins = pin_cols(cols, pinned);
    let reserve = pin_reserve(cols, &pins, widths, max_col);
    let slot = |ci: usize| COLUMN_SPACING + width_of(&cols[ci], ci, widths, max_col);

    // A pinned column is on screen at every offset, so there is no offset that
    // brings it into view and none to compute: answer for the nearest column
    // that does scroll instead.
    let Some(target) = (0..=target).rev().find(|ci| !pins.contains(ci)) else {
        return 0;
    };

    let Some(mut budget) = inner_w
        .saturating_sub(row_num_w)
        .saturating_sub(reserve)
        .checked_sub(slot(target))
    else {
        return target; // the target alone does not fit — show it at the left edge
    };

    // Walk left from the target, taking every column that still fits. A pinned
    // column on the way is stepped over: it is already drawn and already paid
    // for out of `reserve`, so it neither costs the walk nor stops it.
    let mut offset = target;
    for ci in (0..target).rev() {
        if pins.contains(&ci) {
            continue;
        }
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

        // ── Phase 0: the pinned block ─────────────────────────────────────
        // Pinned columns are drawn at the left edge whatever the offset is, so
        // they lead `vis_cols` and are then skipped by the scrolling walk — a
        // column that is both pinned and scrolled to must not be drawn twice.
        let pins = pin_cols(cols, self.pinned);
        let n_pinned = pins.len();
        let mut vis_cols: Vec<usize> = pins.clone();
        let mut consumed = row_num_w;
        let want = |i: usize| {
            if self.widths.contains_key(&i) {
                naturals[i]
            } else {
                naturals[i].min(max_col)
            }
        };
        // Each visible column's width before slack is handed back, kept as the
        // columns are picked: a forced last column can be squeezed below what
        // it asks for, and recomputing the list afterwards would lose that.
        let mut vis_widths: Vec<usize> = pins.iter().map(|&i| want(i)).collect();
        consumed += vis_widths.iter().map(|w| sp + w).sum::<usize>();
        if n_pinned > 0 {
            consumed += PIN_DIVIDER;
        }

        // ── Phase 1: greedily pick visible columns ────────────────────────
        // Row num slot = row_num_w (includes the │ char at end).
        // Each data column slot = sp + col_w.
        for i in self.col_offset..naturals.len() {
            if pins.contains(&i) {
                continue;
            }
            let mut w = want(i);
            if consumed + sp + w > inner_w {
                if vis_cols.len() > n_pinned {
                    break;
                }
                // One scrolling column is drawn even where it does not fit, as
                // one too-wide column always was — a view showing nothing but
                // its pinned block could not be moved through. It is squeezed
                // into what is left rather than overflowing, because the space
                // an overflow takes comes out of the pinned columns, undoing
                // the one thing they were set to do.
                w = inner_w.saturating_sub(consumed + sp).max(MIN_COL_WIDTH);
            }
            vis_cols.push(i);
            vis_widths.push(w);
            consumed += sp + w;
        }

        if vis_cols.is_empty() {
            return;
        }

        // The last column of the scrolling run, if it has one. A pinned column
        // is never it: pins are on screen at every offset, so what scrolls must
        // not be decided from them.
        let scroll_last = vis_cols[n_pinned..].last().copied();

        // ── Phase 2: redistribute leftover space to capped columns ────────
        let capped = vis_widths;
        let slack = inner_w.saturating_sub(consumed);
        // A sorted column asks for its indicator on top of its name, so the
        // slack goes there first. A width set by hand asks for exactly what
        // was set and no more — `z>` is the user saying how wide, and a sort
        // must not talk them out of it.
        let vis_naturals: Vec<usize> = vis_cols
            .iter()
            .map(|&i| {
                if self.widths.contains_key(&i) {
                    naturals[i]
                } else {
                    naturals[i] + indicator_width(self.sort, i)
                }
            })
            .collect();
        let final_widths = redistribute(capped, &vis_naturals, slack);

        // Actual pixels used by full columns (excluding the filler slot).
        let used: usize = row_num_w
            + if n_pinned > 0 { PIN_DIVIDER } else { 0 }
            + final_widths.iter().map(|w| sp + w).sum::<usize>();

        // Remaining space for a partial right-edge column. It is the next
        // column the scrolling run would have reached, so a pinned one is
        // stepped over — it is already on screen further left.
        let remaining = inner_w.saturating_sub(used);
        let after = scroll_last.map_or(self.col_offset, |i| i + 1);
        let next_col_idx = (after..cols.len())
            .find(|ci| !pins.contains(ci))
            .unwrap_or(cols.len());
        // The partial column carries the same lead-in as a full one. Without
        // it the last full column's header runs straight into this one's and
        // the two read as a single strange name.
        let has_partial = remaining >= sp + MIN_COL_WIDTH && next_col_idx < cols.len();
        let partial_width = if has_partial { remaining - sp } else { 0 };

        // ── Build ratatui constraints ─────────────────────────────────────
        // Spacing baked into constraints → cursor bg fills the full row.
        // Layout: [row_num_w] [sp+col0] [sp+col1] ... [Min(0) filler]
        let mut widths: Vec<Constraint> = Vec::with_capacity(vis_cols.len() + 3);
        widths.push(Constraint::Length(row_num_w as u16));
        for (idx, &w) in final_widths.iter().enumerate() {
            widths.push(Constraint::Length((sp + w) as u16));
            if idx + 1 == n_pinned {
                widths.push(Constraint::Length(PIN_DIVIDER as u16));
            }
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

            let marks = sort_entry.map(|(order, key)| sort_marks(order, key.1));
            let (name, indicator) =
                header_parts(cols[ci].name().as_str(), final_widths[idx], marks.clone());

            let cell = if let (Some(tick), Some((full, _))) = (self.sort_tick, marks.as_ref()) {
                // Sort in progress: animate the indicator with cycling bold.
                let active = Style::new()
                    .fg(fg)
                    .bold()
                    .add_modifier(Modifier::UNDERLINED);
                let quiet = Style::new()
                    .fg(fg)
                    .add_modifier(Modifier::UNDERLINED)
                    .remove_modifier(Modifier::BOLD);
                let lead = Span::styled(format!("{:>width$}{}", "", name, width = sp), cell_style);

                let spans = if &indicator == full {
                    // " [▲N]" in full: cycle the bold over "[", the arrow, "]".
                    let bold_pos = (tick / 3) % 3;
                    let mut chars = full.chars();
                    chars.next(); // the leading space, carried by the " " span below
                    let bracket = chars.next().map(String::from).unwrap_or_default();
                    let arrow = chars.next().map(String::from).unwrap_or_default();
                    let close = chars.next_back().map(String::from).unwrap_or_default();
                    let num: String = chars.collect();
                    vec![
                        lead,
                        Span::styled(" ", quiet),
                        Span::styled(bracket, if bold_pos == 0 { active } else { quiet }),
                        Span::styled(arrow, if bold_pos == 1 { active } else { quiet }),
                        Span::styled(num, quiet),
                        Span::styled(close, if bold_pos == 2 { active } else { quiet }),
                    ]
                } else {
                    // Too narrow for the brackets, so the arrow alone carries
                    // the animation — blinking rather than cycling.
                    let on = (tick / 3) % 2 == 0;
                    vec![
                        lead,
                        Span::styled(indicator.clone(), if on { active } else { quiet }),
                    ]
                };
                Cell::new(Line::from(spans)).style(cell_style)
            } else {
                let padded = format!("{:>width$}{}{}", "", name, indicator, width = sp);
                Cell::new(padded).style(cell_style)
            };
            header_cells.push(cell);
            // Close the pinned block. The gutter already ends in a │, and
            // without a second one a pinned column reads as sitting next to
            // the column beside it, which it does not.
            if idx + 1 == n_pinned {
                header_cells.push(Cell::new("│").style(hdr_style));
            }
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
                    // The divider takes the gutter's style, so the cursor row's
                    // bar runs through it rather than being broken by it.
                    if idx + 1 == n_pinned {
                        cells.push(Cell::new("│").style(num_style));
                    }
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
            .set(scroll_last.unwrap_or(self.col_offset));

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
            pinned: &Pinned::new(),
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
        draw_pinned(df, width, col_offset, &Pinned::new())
    }

    fn draw_pinned(df: &DataFrame, width: u16, col_offset: usize, pinned: &Pinned) -> Buffer {
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
            pinned,
        }
        .render(area, &mut buf);
        buf
    }

    /// A frame of narrow columns, wide enough that scrolling has somewhere to go.
    fn pin_df() -> DataFrame {
        df! {
            "id"  => ["r1"],
            "aa"  => ["1"],
            "bb"  => ["2"],
            "cc"  => ["3"],
            "dd"  => ["4"],
            "ee"  => ["5"],
        }
        .unwrap()
    }

    /// The whole point: the pinned column is still there after the view has
    /// scrolled past where it lives.
    #[test]
    fn a_pinned_column_stays_on_screen_when_the_view_scrolls_past_it() {
        let df = pin_df();
        let pins: Pinned = [0].into_iter().collect();

        let plain = lines(&draw_narrow(&df, 40, 3))[1].clone();
        assert!(!plain.contains("id"), "unpinned it scrolls away: {plain}");

        let held = lines(&draw_pinned(&df, 40, 3, &pins))[1].clone();
        assert!(held.contains("id"), "pinned it stays: {held}");
        assert!(held.contains("cc"), "and the scrolled columns still show");
        assert!(
            held.find("id") < held.find("cc"),
            "at the left edge, ahead of them: {held}"
        );
    }

    /// A pinned column is drawn once. It leads the row *and* falls inside the
    /// scrolling range at a low offset, so the run has to skip it.
    #[test]
    fn a_pinned_column_that_is_scrolled_to_is_not_drawn_twice() {
        let df = pin_df();
        let pins: Pinned = [1].into_iter().collect();
        let header = lines(&draw_pinned(&df, 40, 0, &pins))[1].clone();
        assert_eq!(header.matches("aa").count(), 1, "{header}");
    }

    /// Without a divider a pinned column reads as sitting beside the column
    /// next to it, which is the one thing it is not.
    #[test]
    fn the_pinned_block_is_closed_by_a_divider() {
        let df = pin_df();
        let pins: Pinned = [0].into_iter().collect();
        let drawn = lines(&draw_pinned(&df, 40, 3, &pins))[1].clone();
        // Inside the block's own borders: one │ closes the row-number gutter,
        // a second closes the pins.
        let chars: Vec<char> = drawn.chars().collect();
        let header: String = chars[1..chars.len() - 1].iter().collect();
        assert_eq!(header.matches('│').count(), 2, "{header}");
        let bar = header.rfind('│').unwrap();
        assert!(
            header[..bar].contains("id") && !header[bar..].contains("id"),
            "the pins sit ahead of the divider and nothing else does: {header}"
        );
    }

    /// The pinned block costs width, so fewer columns fit beside a target and
    /// the offset that shows it at the right edge moves right.
    #[test]
    fn the_offset_that_shows_a_column_pays_for_the_pinned_block() {
        let df = pin_df();
        let bare = col_offset_showing(&df, 0, 40, 5, &Widths::new(), &Pinned::new());
        let held = col_offset_showing(&df, 0, 40, 5, &Widths::new(), &[0].into_iter().collect());
        assert!(held > bare, "bare {bare}, pinned {held}");
    }

    /// A pinned column is on screen at every offset, so there is no offset to
    /// compute for one — asking answers for the nearest column that scrolls.
    #[test]
    fn a_pinned_target_is_answered_by_the_nearest_scrolling_column() {
        let df = pin_df();
        let pins: Pinned = [5].into_iter().collect();
        assert_eq!(
            col_offset_showing(&df, 0, 40, 5, &Widths::new(), &pins),
            col_offset_showing(&df, 0, 40, 4, &Widths::new(), &pins),
        );
        let all: Pinned = (0..6).collect();
        assert_eq!(col_offset_showing(&df, 0, 40, 5, &Widths::new(), &all), 0);
    }

    /// Pin the whole width and there is nothing left to scroll in.
    #[test]
    fn a_pinned_block_that_fills_the_screen_does_not_fit() {
        let df = pin_df();
        assert!(pin_fits(&df, 0, 40, &Widths::new(), &Pinned::new()));
        assert!(pin_fits(
            &df,
            0,
            40,
            &Widths::new(),
            &[0].into_iter().collect()
        ));
        assert!(!pin_fits(&df, 0, 40, &Widths::new(), &(0..6).collect()));
    }

    fn draw_sorted(df: &DataFrame, width: u16, sort: &[(usize, bool)]) -> Buffer {
        let theme = Theme::catppuccin_mocha();
        let last_vis = std::cell::Cell::new(0);
        let area = Rect::new(0, 0, width, 4);
        let mut buf = Buffer::empty(area);
        DataTable {
            df,
            col_offset: 0,
            cursor_col: 0,
            row_offset: 0,
            cursor_row: 0,
            selection_mode: SelectionMode::Cell,
            theme: &theme,
            search: None,
            search_col: None,
            last_vis_col_out: &last_vis,
            sort,
            sort_tick: None,
            edited: &[],
            selection: None,
            relative_rows: true,
            widths: &Widths::new(),
            pinned: &Pinned::new(),
        }
        .render(area, &mut buf);
        buf
    }

    /// The name is what identifies the column. A six-wide `region` used to
    /// render as a bare `…`, every character of it spent on ` [▲1]` — the
    /// sort named, the column lost.
    #[test]
    fn a_sorted_column_does_not_lose_its_name_to_the_indicator() {
        let df = df! { "region" => ["east"], "q1" => ["1"] }.unwrap();
        let header = lines(&draw_sorted(&df, 60, &[(0, true)]))[1].clone();
        assert!(header.contains("region"), "{header}");
        assert!(header.contains('▲'), "and still says it is sorted: {header}");
    }

    /// With the slack to grow into, the column takes the room for the whole
    /// indicator rather than eating into its own name.
    #[test]
    fn a_sorted_column_asks_for_room_for_its_indicator() {
        let df = df! { "region" => ["east"], "q1" => ["1"] }.unwrap();
        let plain = lines(&draw_sorted(&df, 60, &[]))[1].clone();
        let sorted = lines(&draw_sorted(&df, 60, &[(0, true)]))[1].clone();
        assert!(!plain.contains('['), "unsorted has no indicator: {plain}");
        assert!(sorted.contains("region [▲1]"), "{sorted}");
    }

    /// Squeezed with nowhere to grow, the indicator gives way before the name
    /// does. Tested on `header_parts` rather than through a render, because
    /// the interesting case is the one where the layout has no slack left and
    /// arranging for that through the width arithmetic tests the arithmetic,
    /// not the ladder.
    #[test]
    fn a_squeezed_indicator_gives_up_its_brackets_before_the_name() {
        let marks = || Some(sort_marks(0, true));

        // Room for both: nothing gives way.
        assert_eq!(
            header_parts("region", 11, marks()),
            ("region".to_string(), " [▲1]".to_string())
        );

        // One short of the full indicator: the brackets and the priority go,
        // the arrow and the whole name stay.
        assert_eq!(
            header_parts("region", 10, marks()),
            ("region".to_string(), "▲".to_string())
        );
        assert_eq!(
            header_parts("region", 7, marks()),
            ("region".to_string(), "▲".to_string())
        );

        // Past that the name is truncated, but never below what is left after
        // the arrow — and never to the bare `…` this used to render.
        let (name, indicator) = header_parts("region", 6, marks());
        assert_eq!(indicator, "▲", "the arrow is the part that cannot go");
        assert_eq!(name, "regi…", "and the name keeps the rest");
    }

    /// The priority number is what tells a multi-key sort apart, so it is only
    /// dropped under real pressure.
    #[test]
    fn the_indicator_keeps_its_priority_number_when_there_is_room() {
        assert_eq!(sort_marks(2, false).0, " [▼3]");
        assert_eq!(
            header_parts("q1", 8, Some(sort_marks(2, false))),
            ("q1".to_string(), " [▼3]".to_string())
        );
    }

    /// An unsorted column is untouched by any of this.
    #[test]
    fn an_unsorted_header_is_just_its_name() {
        assert_eq!(
            header_parts("region", 20, None),
            ("region".to_string(), String::new())
        );
        assert_eq!(header_parts("region", 4, None).0, "reg…");
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
