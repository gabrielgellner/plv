use std::{
    cell::Cell,
    io,
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout},
    style::Style,
    widgets::Paragraph,
};

use crate::complete::{self, Completion};
use crate::data::Store;
use crate::data::lake_db::{self, LakeDb};
use crate::lake::{Lake, Level, Scope};
use crate::picker::Picker;
use crate::search::{SearchQuery, SearchState, SearchStatus};
use crate::ui::{
    self, Browser, CellView, DataTable, Help, Panel, Prompt, Section, SelectionMode, StatusBar,
    Theme,
};
use crate::view;
use polars::prelude::DataType;

enum AppMode {
    Normal,
    Search,
    /// Typing a new value for one cell. What is typed goes into the edit
    /// buffer, never straight to disk — `:w` writes.
    Edit,
    /// Typing an ex command after `:`.
    Command,
    /// The column picker is up. It holds a working copy of what it edits, so
    /// the table underneath is untouched until `Enter`.
    Picker,
}

/// A rectangle of cells, as inclusive `(row range, column range)` in absolute
/// coordinates.
type Block = ((usize, usize), (usize, usize));

/// A block operator holds a value per cell — an overlay entry, or a register
/// slot — and a column-mode selection covers every row in the file. Past this
/// many cells it is refused rather than attempted.
const MAX_BLOCK: usize = 100_000;

/// A pending operator over a visual selection.
#[derive(Clone, Copy)]
struct Fill {
    block: Block,
    mode: FillMode,
}

/// How a fill combines the typed text with what each cell already holds.
///
/// Mirrors blockwise visual mode in vim, where `c` changes the block outright
/// but `I` and `A` insert at the start and the end of every line in it. plv has
/// no text objects, so the lowercase keys are free to mean the same thing.
#[derive(Clone, Copy, PartialEq, Debug)]
enum FillMode {
    /// `c`: the typed value replaces each cell.
    Replace,
    /// `i`: the typed text goes in front of what is already there.
    Prepend,
    /// `a`: the typed text goes after what is already there.
    Append,
}

impl FillMode {
    /// How the prompt describes what is about to happen.
    fn verb(self) -> &'static str {
        match self {
            FillMode::Replace => "fill",
            FillMode::Prepend => "prepend to",
            FillMode::Append => "append to",
        }
    }

    fn combine(self, typed: &str, current: &str) -> String {
        match self {
            FillMode::Replace => typed.to_string(),
            FillMode::Prepend => format!("{typed}{current}"),
            FillMode::Append => format!("{current}{typed}"),
        }
    }
}

/// How far `Ctrl+d`/`Ctrl+u` and `Ctrl+f`/`Ctrl+b` move.
#[derive(Clone, Copy)]
enum Page {
    Half,
    Whole,
}

/// Where the caret lands when a cell edit opens, following vim.
#[derive(Clone, Copy)]
enum EditStart {
    /// `i`: the value as it stands, caret at the front.
    Front,
    /// `a`: the value as it stands, caret at the end.
    End,
    /// `c`: an empty field.
    Empty,
}

/// Which screen is in front: the lake catalog browser, or the data viewer.
#[derive(PartialEq)]
enum Screen {
    Browser,
    Viewer,
}

pub struct App {
    file_path: Option<PathBuf>,
    store: Option<Store>,
    col_offset: usize,
    cursor_row: usize,
    pending_num: String,
    /// A multi-key prefix waiting for its second key: `g` or `z`.
    pending_prefix: Option<char>,
    message: Option<String>,
    exit: bool,
    error: Option<String>,
    theme: Theme,
    selection_mode: SelectionMode,
    /// Selected column (absolute index). Only actively navigated in Column/Cell modes;
    /// initialised to col_offset when entering those modes via Tab.
    cursor_col: usize,
    /// Last fully-visible column index from the most recent render. Used to
    /// detect when the column cursor has scrolled off the right edge.
    last_vis_col: usize,
    /// Terminal width captured each frame; used to compute column layout.
    last_frame_width: u16,
    mode: AppMode,
    /// Cell being edited, as `(absolute row, column index)`. Editing is
    /// blocked while a sort is active, so the row is also the file's row.
    edit_cell: Option<(usize, usize)>,
    /// When set, committing runs over a whole block rather than over
    /// `edit_cell` alone.
    edit_fill: Option<Fill>,
    /// Where a visual selection started, as `(absolute row, column index)`.
    /// Absolute, so scrolling the anchor off screen does not move it.
    visual_anchor: Option<(usize, usize)>,
    /// The yank register: a block of cell text, rows outermost. Internal to
    /// plv — nothing is exchanged with the system clipboard.
    register: Vec<Vec<String>>,
    edit_buf: String,
    /// Caret position in `edit_buf`, counted in characters.
    edit_cursor: usize,
    command_buf: String,
    /// Candidates from the last Tab on the command line, and which of them is
    /// currently applied. Cleared by any key that changes the line.
    completion: Option<Completion>,
    completion_index: Option<usize>,
    search_buf: String,
    search_state: Option<SearchState>,
    /// Receives batches of matching row indices from the background search thread.
    /// Dropping this cancels the search.
    search_rx: Option<mpsc::Receiver<Vec<usize>>>,
    /// Receives the sorted first-page DataFrame from the background sort thread.
    sort_rx: Option<mpsc::Receiver<polars::prelude::DataFrame>>,
    /// Receives batches of rows matching the active `:filter`. Dropping this
    /// cancels the scan.
    filter_rx: Option<mpsc::Receiver<Vec<usize>>>,
    /// Incremented each draw while a background task is running; drives animations.
    spinner_tick: usize,
    /// Present when the opened path was a DuckLake catalog rather than a file.
    lake: Option<Lake>,
    screen: Screen,
    /// Viewport height captured each frame, so key handlers can size a new Store.
    last_vp: usize,
    /// The `?` key-binding overlay is showing.
    help_visible: bool,
    /// Number rows by distance from the cursor, so `{n}j` and `{n}G` can be
    /// read off the gutter instead of worked out.
    relative_rows: bool,
    /// The cursor cell is being shown in full above the status bar.
    ///
    /// Stays on while the cursor moves, so a column of long values can be read
    /// by walking down it — which is the thing a truncated column makes hard.
    cell_view: bool,
    /// Column widths set by hand, by source column index. Kept here and not
    /// in the `View`: how wide a column is drawn is a fact about this screen,
    /// not about which rows and columns the file is being asked for.
    widths: ui::Widths,
    /// Columns held at the left edge while the rest scroll past, by **source**
    /// column index — so a pin follows its column through a `:select` that
    /// reorders, exactly as `widths` does.
    ///
    /// Display state for the same reason widths are, and out of the `View` for
    /// the same reason: which column stays in sight while you walk sideways is
    /// a fact about this screen. `u` is the edit buffer's undo and does not
    /// take a pin back; `zp` again does, and `z|` takes them all back.
    pinned: std::collections::BTreeSet<usize>,
    /// The column picker, while it is up.
    picker: Option<Picker>,
}

impl App {
    pub fn new(file: Option<PathBuf>) -> Self {
        Self {
            file_path: file,
            store: None,
            col_offset: 0,
            cursor_row: 0,
            pending_num: String::new(),
            pending_prefix: None,
            message: None,
            exit: false,
            error: None,
            theme: Theme::catppuccin_mocha(),
            selection_mode: SelectionMode::default(),
            cursor_col: 0,
            last_vis_col: 0,
            last_frame_width: 0,
            mode: AppMode::Normal,
            edit_cell: None,
            edit_fill: None,
            visual_anchor: None,
            register: Vec::new(),
            edit_buf: String::new(),
            edit_cursor: 0,
            command_buf: String::new(),
            completion: None,
            completion_index: None,
            search_buf: String::new(),
            search_state: None,
            search_rx: None,
            sort_rx: None,
            filter_rx: None,
            spinner_tick: 0,
            lake: None,
            screen: Screen::Viewer,
            last_vp: 20,
            help_visible: false,
            relative_rows: true,
            cell_view: false,
            widths: ui::Widths::new(),
            pinned: std::collections::BTreeSet::new(),
            picker: None,
        }
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
        if let Some(path) = self.file_path.clone() {
            let size = terminal.size()?;
            let vp = Self::viewport_rows(size.height, 0);
            self.last_vp = vp;

            if let Some(lake_path) = lake_db::detect(&path) {
                match LakeDb::open(&lake_path, None).and_then(Lake::new) {
                    Ok(lake) => {
                        self.lake = Some(lake);
                        self.screen = Screen::Browser;
                    }
                    Err(e) => self.error = Some(e.to_string()),
                }
            } else {
                match Store::open_file(&path, vp) {
                    Ok(store) => self.store = Some(store),
                    Err(e) => self.error = Some(e.to_string()),
                }
            }
        }

        while !self.exit {
            terminal.draw(|frame| self.draw(frame))?;
            self.handle_events()?;
        }
        Ok(())
    }

    /// status bar (1) + block borders (2) + header row (1) = 4 overhead,
    /// plus whatever the candidate panel is taking.
    fn viewport_rows(terminal_height: u16, panel: u16) -> usize {
        (terminal_height as usize).saturating_sub(4 + panel as usize)
    }

    /// Rows the strip above the status bar wants right now.
    ///
    /// Completion and the cell view share it. They cannot both be wanted —
    /// one belongs to the command line and the other to the table — so
    /// whichever is on gets it.
    fn panel_height(&self, width: u16, height: u16) -> u16 {
        if let Some(completion) = &self.completion {
            return ui::panel_height(&completion.options, width);
        }
        if self.cell_view {
            // Never more than half the screen: the table is still the point.
            let room = (height / 2).max(2);
            return match self.cell_under_cursor() {
                Some((_, value)) => ui::cell_height(&value, width, room),
                None => 0,
            };
        }
        0
    }

    /// The column name and full text of the cell under the cursor.
    fn cell_under_cursor(&self) -> Option<(String, String)> {
        let store = self.store.as_ref()?;
        let (name, _) = store.column_info(self.cursor_col)?;
        let value = store.cell_text(self.cursor_row, self.cursor_col)?;
        Some((name, value))
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        self.last_frame_width = area.width;

        let panel = self.panel_height(area.width, area.height);
        let vp = Self::viewport_rows(area.height, panel);
        self.last_vp = vp;
        if let Some(s) = &mut self.store
            && s.viewport_rows != vp
        {
            let _ = s.resize(vp);
        }

        if self.screen == Screen::Browser {
            self.draw_browser(frame, area);
            self.draw_help(frame, area);
            return;
        }

        // A resize can leave row mode scrolled past what now fits.
        if matches!(self.selection_mode, SelectionMode::Row) {
            self.col_offset = self.col_offset.min(self.max_col_offset());
        }

        // In Column/Cell mode keep cursor_col within [col_offset, last_vis_col].
        // Must be correct in a single pass: handle_events blocks on event::read
        // when no search is active, so multi-frame convergence never fires.
        if !matches!(self.selection_mode, SelectionMode::Row) {
            // A pinned column is drawn at every offset, so landing on one is
            // never a reason to scroll: the view stays where it was and the
            // cursor is visible in the pinned block.
            let held = self.display_pins().contains(&self.cursor_col);
            if !held && self.cursor_col < self.col_offset {
                self.col_offset = self.cursor_col;
            } else if !held && self.cursor_col > self.last_vis_col {
                // cursor is off the right edge — compute the col_offset that
                // places cursor_col at the rightmost visible position.
                self.col_offset = self.col_offset_to_show_at_right(self.cursor_col);
            }
        }

        let [table_area, panel_area, status_area] = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(panel),
            Constraint::Length(1),
        ])
        .areas(area);

        // Done here, while `self` is still free to be borrowed mutably: the
        // render below only reads.
        if let Some(picker) = &mut self.picker {
            let len = picker.len();
            picker.state.go_to(picker.state.selected, len);
            picker
                .state
                .clamp_scroll(Browser::viewport_rows(table_area.height));
        }

        let file_name = self
            .lake
            .as_ref()
            .and_then(|l| l.scope_label())
            .or_else(|| {
                self.file_path
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "plv".to_string());
        let col_offset = self.col_offset;
        let cursor_row = self.cursor_row;

        let vis_col_cell = Cell::new(self.col_offset);

        if let Some(store) = &self.store {
            // Advance spinner each frame while any background task is running.
            if self.search_rx.is_some() || self.sort_rx.is_some() || self.filter_rx.is_some() {
                self.spinner_tick = self.spinner_tick.wrapping_add(1);
            }

            let cursor_col = match self.selection_mode {
                SelectionMode::Row => col_offset,
                _ => self.cursor_col,
            };
            let edited = store.edited_cells();
            let sort_display = store.sort_display();

            // The picker takes the table's place rather than covering it:
            // the status bar below stays put, so a refusal it makes is
            // reported where every other refusal is.
            if let Some(picker) = &self.picker {
                let rows = picker.rows();
                frame.render_widget(
                    Browser {
                        title: picker.title(),
                        headers: Picker::headers(),
                        widths: Picker::widths(),
                        rows: &rows,
                        selected: picker.state.selected,
                        offset: picker.state.offset,
                        theme: &self.theme,
                    },
                    table_area,
                );
            } else {
                frame.render_widget(
                    DataTable {
                        df: &store.current_view,
                        col_offset,
                        cursor_col,
                        row_offset: store.row_offset,
                        cursor_row,
                        selection_mode: self.selection_mode,
                        theme: &self.theme,
                        search: self.search_state.as_ref(),
                        search_col: self.search_state.as_ref().and_then(|s| s.col_idx),
                        last_vis_col_out: &vis_col_cell,
                        sort: &sort_display,
                        sort_tick: self.sort_rx.as_ref().map(|_| self.spinner_tick),
                        edited: &edited,
                        selection: self.visual_range(),
                        relative_rows: self.relative_rows,
                        widths: &self.widths,
                        pinned: &self.display_pins(),
                    },
                    table_area,
                );
            }

            match self.mode {
                AppMode::Search => {
                    frame.render_widget(
                        Prompt {
                            prefix: "/",
                            buffer: &self.search_buf,
                            cursor: self.search_buf.chars().count(),
                            theme: &self.theme,
                        },
                        status_area,
                    );
                }
                AppMode::Command => {
                    frame.render_widget(
                        Prompt {
                            prefix: ":",
                            buffer: &self.command_buf,
                            cursor: self.command_buf.chars().count(),
                            theme: &self.theme,
                        },
                        status_area,
                    );
                }
                AppMode::Edit => {
                    // Name the column being edited: once the prompt has the
                    // caret, the header is the only thing saying what the
                    // value means.
                    // A fill spans columns, so it is counted rather than
                    // named; a single cell is named by its column.
                    let label = match self.edit_fill {
                        Some(fill) => format!(
                            " {} {} cells: ",
                            fill.mode.verb(),
                            Self::block_size(fill.block)
                        ),
                        None => self
                            .edit_cell
                            .and_then(|(_, col)| store.column_info(col))
                            .map_or_else(|| " ".to_string(), |(name, _)| format!(" {name}: ")),
                    };
                    frame.render_widget(
                        Prompt {
                            prefix: &label,
                            buffer: &self.edit_buf,
                            cursor: self.edit_cursor,
                            theme: &self.theme,
                        },
                        status_area,
                    );
                }
                AppMode::Normal | AppMode::Picker => {
                    let search_info = self.search_state.as_ref().map(|s| {
                        let (cur, total, complete) = s.match_info();
                        (s.query.raw.clone(), cur, total, complete)
                    });
                    frame.render_widget(
                        StatusBar {
                            file_name,
                            cursor_row,
                            total_rows: store.row_count(),
                            view: store.view.describe(&store.schema).map(|text| {
                                if store.filter_truncated() {
                                    format!("{text}  (first {})", store.row_count())
                                } else {
                                    text
                                }
                            }),
                            col_position: self.col_position(),
                            total_cols: store.column_count(),
                            message: self.message.clone(),
                            dirty: store.dirty(),
                            selection: self
                                .visual_range()
                                .map(|((r0, r1), (c0, c1))| (r1 - r0 + 1, c1 - c0 + 1)),
                            pending_num: self.pending_num.clone(),
                            pending_prefix: self.pending_prefix,
                            theme: &self.theme,
                            search_info,
                            spinner_tick: self.spinner_tick,
                            sort_tick: self.sort_rx.as_ref().map(|_| self.spinner_tick),
                            filtering: store.filtering(),
                            help: if self.picker.is_some() {
                                " -:show  a/A:all/one  p:pin  ⏎:apply  esc "
                            } else if self.lake.is_some() {
                                " f:partitions  T:snapshots  b:back  ?:help "
                            } else {
                                " j/k:↕  h/l:←→  /:search  ?:help  q:quit "
                            },
                        },
                        status_area,
                    );
                }
            }
        } else if let Some(err) = &self.error {
            frame.render_widget(
                Paragraph::new(format!("Error: {err}")).style(Style::new().red()),
                table_area,
            );
        } else {
            frame.render_widget(
                Paragraph::new("Usage: plv <file.csv|file.parquet|lake.ducklake|bundle-dir>")
                    .centered(),
                table_area,
            );
        }

        if let Some(completion) = &self.completion {
            frame.render_widget(
                Panel {
                    items: &completion.options,
                    selected: self.completion_index,
                    theme: &self.theme,
                },
                panel_area,
            );
        } else if self.cell_view
            && let Some((name, value)) = self.cell_under_cursor()
        {
            frame.render_widget(
                CellView {
                    name: &name,
                    value: &value,
                    theme: &self.theme,
                },
                panel_area,
            );
        }

        self.draw_help(frame, area);
        self.last_vis_col = vis_col_cell.get();
    }

    fn draw_help(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        if !self.help_visible {
            return;
        }
        frame.render_widget(
            Help {
                sections: self.help_sections(),
                theme: &self.theme,
            },
            area,
        );
    }

    /// Key bindings for whatever is currently in front.
    fn help_sections(&self) -> &'static [Section<'static>] {
        const MOVE: &[(&str, &str)] = &[
            ("j / \u{2193}", "Move down"),
            ("k / \u{2191}", "Move up"),
            (
                "Ctrl+d / Ctrl+u",
                "Half a screen down / up, view and cursor",
            ),
            ("Ctrl+f / Ctrl+b", "A whole screen down / up"),
            ("gg / G", "First / last row"),
            ("{n}gg / {n}G", "Jump to row n"),
            ("zz / zt / zb", "Centre / top / bottom"),
            ("z> / z<", "Widen / narrow the cursor column"),
            ("z_", "Fit the column to what is on screen"),
            ("z=", "Put every column width back"),
            ("K", "Show the cursor cell in full"),
            ("#", "Relative or absolute row numbers"),
        ];
        const COLUMNS: &[(&str, &str)] = &[
            ("h / l", "Scroll columns left / right"),
            ("{n}h / {n}l", "Jump n columns"),
            ("H", "First column"),
            ("0 / $", "Scroll to the first / last column"),
            ("Tab", "Cycle row \u{2192} column \u{2192} cell"),
            ("s", "Sort by cursor column"),
            ("-", "Hide the cursor column (:reset select brings it back)"),
            ("C", "The column picker: show, hide and pin from a list"),
            ("zp", "Pin / unpin the cursor column at the left edge"),
            ("z|", "Unpin every column"),
        ];
        const SEARCH: &[(&str, &str)] = &[
            ("/", "Search (regex)"),
            ("n / N", "Next / previous match"),
            ("Esc", "Clear search and sorts"),
        ];
        const EDIT: &[(&str, &str)] = &[
            ("i / a", "Edit cell, caret at start / end"),
            ("c", "Replace cell"),
            ("x", "Clear cell"),
            ("dd / {n}dd", "Delete the row, or n rows"),
            ("o / O", "Open a new row below / above"),
            ("u / Ctrl+r", "Undo / redo"),
            ("y / p", "Yank the cursor / paste at the cursor"),
            (
                "Tab / Shift+Tab",
                "While editing: commit, next / previous cell",
            ),
            (":w   :w!   :w path", "Write (force / elsewhere)"),
            (":q   :q!   :wq", "Quit (discarding / writing)"),
        ];
        const VIEWS: &[(&str, &str)] = &[
            (":select a b", "Show only these columns"),
            (":hide a b", "Drop these columns"),
            (":filter c > 10", "Keep matching rows; `~` is a regex"),
            (":filter a = x and b ~ y", "Conditions join with `and`"),
            (":sort a b-", "Sort by columns, `-` for descending"),
            (":select   :sort", "The verb alone puts it back"),
            (":reset [slot]", "Clear select, filter, sort, or all"),
            (
                "Tab / Shift+Tab",
                "Complete, then step through the candidates",
            ),
        ];
        const VISUAL: &[(&str, &str)] = &[
            ("v", "Start or cancel a selection"),
            ("", "Its shape follows the Tab mode"),
            ("c", "Replace every cell with one value"),
            ("i / a", "Prepend / append text to every cell"),
            ("x", "Clear the selected cells"),
            ("d", "Delete the selected rows"),
            ("y", "Yank the selection"),
        ];
        const GENERAL: &[(&str, &str)] = &[("?", "This help"), ("q", "Quit")];
        const LAKE: &[(&str, &str)] = &[
            ("f", "Partitions of this table"),
            ("b", "Back to the catalog"),
            ("T", "Snapshots (time travel)"),
        ];
        const BROWSE: &[(&str, &str)] = &[
            ("j / k", "Move selection"),
            ("g / G", "First / last entry"),
            ("Ctrl+d / Ctrl+u", "Half page down / up"),
            ("Enter", "Open the selection"),
            ("l / f", "Partitions of this table"),
            ("T", "Snapshots (time travel)"),
            ("a", "Whole table (from partitions)"),
            ("h / Esc", "Back"),
        ];

        const VIEWER: &[Section<'static>] = &[
            ("Rows", MOVE),
            ("Columns", COLUMNS),
            ("Search", SEARCH),
            ("General", GENERAL),
        ];
        const VIEWER_LAKE: &[Section<'static>] = &[
            ("Rows", MOVE),
            ("Columns", COLUMNS),
            ("Search", SEARCH),
            ("Lake", LAKE),
            ("General", GENERAL),
        ];
        const VIEWER_EDIT: &[Section<'static>] = &[
            ("Rows", MOVE),
            ("Columns", COLUMNS),
            ("Search", SEARCH),
            ("Edit", EDIT),
            ("Visual", VISUAL),
            ("Views", VIEWS),
            ("General", GENERAL),
        ];
        const PICK: &[(&str, &str)] = &[
            ("j / k", "Move down the list"),
            ("g / G", "First / last column"),
            ("Ctrl+d / Ctrl+u", "Half page down / up"),
            ("-", "Show or hide this column"),
            ("a / A", "Show every column / hide all but this one"),
            ("p", "Pin or unpin this column"),
            ("Enter", "Apply"),
            ("Esc", "Cancel, changing nothing"),
        ];
        const BROWSER: &[Section<'static>] = &[("Catalog", BROWSE), ("General", GENERAL)];
        // No `General` section: its `q` means quit, and in the picker `q`
        // cancels — one overlay must not say both. The picker's own list
        // already covers every key it answers.
        const PICKER: &[Section<'static>] = &[("Columns", PICK)];

        // The status bar has room for a few hints and then degrades, so for a
        // modal screen whose keys are not guessable the overlay is the real
        // reference — which is the reason it exists.
        if self.picker.is_some() {
            PICKER
        } else if self.screen == Screen::Browser {
            BROWSER
        } else if self.lake.is_some() {
            VIEWER_LAKE
        } else if self.store.as_ref().is_some_and(Store::is_editable) {
            VIEWER_EDIT
        } else {
            VIEWER
        }
    }

    // ── lake catalog browser ──────────────────────────────────────────────

    fn draw_browser(&mut self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let [list_area, status_area] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(area);

        let Some(lake) = &mut self.lake else { return };

        let len = lake.list_len();
        lake.state.go_to(lake.state.selected, len);
        lake.state
            .clamp_scroll(Browser::viewport_rows(list_area.height));

        let rows = lake.rows();
        frame.render_widget(
            Browser {
                title: lake.title(),
                headers: lake.headers(),
                widths: lake.widths(),
                rows: &rows,
                selected: lake.state.selected,
                offset: lake.state.offset,
                theme: &self.theme,
            },
            list_area,
        );

        let help = match lake.level {
            Level::Tables => " Enter:open  l:partitions  T:snapshots  ?:help  q:quit ",
            Level::Partitions { .. } => " Enter:open partition  a:whole table  h:back  ?:help ",
            Level::Snapshots => " Enter:travel to snapshot  h:back  ?:help ",
        };
        let line = match &self.message {
            Some(msg) => format!(" {msg}"),
            None => format!("{help}  [{}/{}]", lake.state.selected + 1, len.max(1)),
        };
        let style = if self.message.is_some() {
            Style::new()
                .bg(self.theme.message_bg)
                .fg(self.theme.message_fg)
        } else {
            Style::new()
                .bg(self.theme.status_bg)
                .fg(self.theme.status_fg)
        };
        frame.render_widget(Paragraph::new(line).style(style), status_area);
    }

    fn handle_browser_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(lake) = &mut self.lake else {
            return Ok(());
        };
        let len = lake.list_len();

        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => self.exit = true,
            KeyCode::Char('j') | KeyCode::Down => lake.state.move_by(1, len),
            KeyCode::Char('k') | KeyCode::Up => lake.state.move_by(-1, len),
            KeyCode::Char('d') if ctrl => lake.state.move_by(self.last_vp as isize / 2, len),
            KeyCode::Char('u') if ctrl => lake.state.move_by(-(self.last_vp as isize) / 2, len),
            KeyCode::Char('g') | KeyCode::Home => lake.state.go_to(0, len),
            KeyCode::Char('G') | KeyCode::End => lake.state.go_to(len.saturating_sub(1), len),

            // Descend into the partition list for the selected table.
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Char('f') => {
                if let Level::Tables = lake.level {
                    let table = lake.state.selected;
                    if lake
                        .table(table)
                        .is_some_and(|t| !t.partition_cols.is_empty())
                    {
                        self.show_partitions(table, 0)?;
                    } else {
                        self.message = Some("Table is not partitioned".to_string());
                    }
                }
            }

            // Time travel: list the lake's snapshots.
            KeyCode::Char('T') => {
                lake.level = Level::Snapshots;
                lake.state = Default::default();
                let current = lake.current_snapshot_index();
                lake.state.go_to(current, lake.list_len());
            }

            // Back out of a sub-list to the table list.
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Esc => {
                if let Level::Partitions { table } = lake.level {
                    lake.level = Level::Tables;
                    lake.state = Default::default();
                    lake.state.go_to(table, lake.list_len());
                } else if let Level::Snapshots = lake.level {
                    lake.level = Level::Tables;
                    lake.state = Default::default();
                } else if self.store.is_some() {
                    // Nothing to go back to at the top level; return to the
                    // viewer if one is already open.
                    self.screen = Screen::Viewer;
                }
            }

            // Open the whole table even while standing in its partition list.
            KeyCode::Char('a') => {
                if let Level::Partitions { table } = lake.level {
                    self.open_scope(Scope {
                        table,
                        partition: None,
                    })?;
                }
            }

            KeyCode::Enter => match lake.level {
                Level::Tables => {
                    let table = lake.state.selected;
                    self.open_scope(Scope {
                        table,
                        partition: None,
                    })?;
                }
                Level::Partitions { table } => {
                    let partition = lake.state.selected;
                    self.open_scope(Scope {
                        table,
                        partition: Some(partition),
                    })?;
                }
                Level::Snapshots => {
                    let Some(snapshot) = lake.snapshots.get(lake.state.selected) else {
                        return Ok(());
                    };
                    let (id, time) = (snapshot.id, snapshot.short_time());
                    self.switch_snapshot(id, &time)?;
                }
            },

            _ => {}
        }
        Ok(())
    }

    /// Re-resolve the whole catalog as of `snapshot` and, where possible, stay
    /// on the table the viewer was already showing so the same data can be
    /// compared across snapshots.
    ///
    /// A file-level scope is deliberately widened to the whole table: file ids
    /// are not stable across snapshots, so the same list position would mean a
    /// different slice of data.
    fn switch_snapshot(&mut self, snapshot: i64, time: &str) -> anyhow::Result<()> {
        let Some(lake) = &self.lake else {
            return Ok(());
        };
        let path = lake.db.path.clone();
        let previous = lake
            .scope
            .and_then(|s| lake.table(s.table))
            .map(|t| t.qualified_name());

        let reopened = match LakeDb::open(&path, Some(snapshot)).and_then(Lake::new) {
            Ok(lake) => lake,
            Err(e) => {
                self.message = Some(format!("Cannot read snapshot {snapshot}: {e}"));
                return Ok(());
            }
        };

        let target = previous.and_then(|name| {
            reopened
                .tables
                .iter()
                .position(|t| t.qualified_name() == name)
        });

        self.store = None;
        self.reset_view();
        self.lake = Some(reopened);

        if let Some(table) = target {
            self.open_scope(Scope {
                table,
                partition: None,
            })?;
            if let Some(lake) = &mut self.lake {
                lake.state.go_to(table, lake.list_len());
            }
        }

        // Whether or not the table survived, say where we landed. This
        // overwrites any inlined-rows notice from open_scope — the snapshot
        // change is the more important thing to report right now.
        self.message = Some(format!("Snapshot {snapshot} ({time})"));
        Ok(())
    }

    /// Show the partition list for `table`, loading it if needed.
    ///
    /// Partition lists are not loaded up front: each one costs a GROUP BY over
    /// the table, so it is paid only when the user asks to see it.
    fn show_partitions(&mut self, table: usize, select: usize) -> anyhow::Result<()> {
        let Some(lake) = &mut self.lake else {
            return Ok(());
        };
        if let Err(e) = lake.load_partitions(table) {
            self.message = Some(format!("Cannot list partitions: {e}"));
            return Ok(());
        }
        lake.level = Level::Partitions { table };
        lake.state = Default::default();
        lake.state.go_to(select, lake.list_len());
        self.screen = Screen::Browser;
        Ok(())
    }

    /// Build a `Store` for `scope` and switch to the data viewer.
    ///
    /// Errors are surfaced as a status message rather than aborting: a bad
    /// scope should leave the user in the browser, able to pick another.
    fn open_scope(&mut self, scope: Scope) -> anyhow::Result<()> {
        let vp = self.last_vp;
        let Some(lake) = &self.lake else {
            return Ok(());
        };
        let Some(table) = lake.table(scope.table) else {
            return Ok(());
        };

        // Row counts come from the partition list or the table's own count,
        // both of which the extension answers from catalog statistics.
        let (partition, rows) = match scope.partition {
            Some(i) => match lake.partitions.get(i) {
                Some(p) => (Some(p), p.rows),
                None => return Ok(()),
            },
            None => (None, table.rows),
        };

        let source = lake.db.source(table, partition);
        let opened = lake
            .db
            .try_clone()
            .and_then(|conn| Store::new_lake(conn, source, vp, rows as usize));

        match opened {
            Ok(store) => {
                self.store = Some(store);
                self.reset_view();
                if let Some(lake) = &mut self.lake {
                    lake.scope = Some(scope);
                }
                self.screen = Screen::Viewer;
            }
            Err(e) => self.message = Some(format!("Cannot open: {e}")),
        }
        Ok(())
    }

    /// Clear per-table view state when a different scope is loaded.
    fn reset_view(&mut self) {
        self.col_offset = 0;
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.pending_num.clear();
        self.search_state = None;
        self.search_rx = None;
        self.sort_rx = None;
        self.filter_rx = None;
    }

    fn handle_events(&mut self) -> io::Result<()> {
        // If a background task changed any state, return immediately so the
        // main loop redraws before blocking on input.
        if self.poll_sort() || self.poll_search() || self.poll_filter() {
            return Ok(());
        }

        // While any background task is running use a short timeout so the UI
        // redraws as result batches arrive. When idle, block on read directly.
        if (self.search_rx.is_some() || self.sort_rx.is_some() || self.filter_rx.is_some())
            && !event::poll(Duration::from_millis(50))?
        {
            return Ok(());
        }

        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && let Err(e) = self.handle_key_event(key)
        {
            self.error = Some(e.to_string());
        }
        Ok(())
    }

    /// Drain any pending search result batches from the background thread,
    /// then auto-jump to the nearest match on the first batch that arrives.
    /// Returns `true` if any state changed (triggers an immediate redraw).
    fn poll_search(&mut self) -> bool {
        let mut changed = false;

        loop {
            let result = match &self.search_rx {
                None => break,
                Some(rx) => rx.try_recv(),
            };
            match result {
                Ok(rows) => {
                    // What the search reports depends on the frame it read.
                    let shown: Vec<usize> = match &self.store {
                        Some(store) => rows
                            .into_iter()
                            .filter_map(|r| store.search_row_to_display(r))
                            .collect(),
                        None => rows,
                    };
                    if let Some(state) = &mut self.search_state {
                        state.matching_rows.extend(shown);
                        changed = true;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.search_rx = None;
                    if let Some(state) = &mut self.search_state {
                        state.status = SearchStatus::Complete;
                    }
                    changed = true;
                    break;
                }
            }
        }

        // Auto-jump to nearest match the first time results arrive.
        let needs_jump = self
            .search_state
            .as_ref()
            .is_some_and(|s| !s.initial_jump_done && !s.matching_rows.is_empty());

        if needs_jump {
            let cursor = self.cursor_row;
            let row = self.search_state.as_mut().and_then(|s| {
                s.initial_jump_done = true;
                s.next_from(cursor)
            });
            if let Some(row) = row {
                let _ = self.cursor_to(row);
            }
        }

        changed
    }

    /// Take whatever the filter scan has found since the last frame.
    fn poll_filter(&mut self) -> bool {
        let Some(rx) = &self.filter_rx else {
            return false;
        };
        let mut batches = Vec::new();
        let mut finished = false;
        loop {
            match rx.try_recv() {
                Ok(batch) => batches.push(batch),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
            }
        }
        if batches.is_empty() && !finished {
            return false;
        }
        if let Some(store) = &mut self.store {
            for batch in batches {
                // The set fills up before the file runs out on a filter that
                // matches nearly everything; stop asking for more.
                if !store.extend_filter(batch).unwrap_or(false) {
                    finished = true;
                    break;
                }
            }
            if finished {
                let _ = store.finish_filter();
            }
        }
        if finished {
            // Dropping the receiver ends the scan.
            self.filter_rx = None;
        }
        // Rows arriving can leave the cursor past the end of what matched.
        let last = self
            .store
            .as_ref()
            .map_or(0, |s| s.row_count().saturating_sub(1));
        self.cursor_row = self.cursor_row.min(last);
        true
    }

    /// Check for completion of the background sort thread.
    /// Returns `true` if state changed (triggers an immediate redraw).
    fn poll_sort(&mut self) -> bool {
        let result = match &self.sort_rx {
            None => return false,
            Some(rx) => rx.try_recv(),
        };
        match result {
            Ok(df) => {
                self.sort_rx = None;
                if let Some(store) = &mut self.store {
                    let _ = store.adopt_sorted(df);
                }
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.sort_rx = None;
                true
            }
        }
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        self.message = None;
        if self.help_visible {
            // Any key dismisses the overlay, including a second '?'.
            self.help_visible = false;
            return Ok(());
        }
        // Only in the modes that are not taking text: '?' is an ordinary
        // character to type into a search pattern, a cell or a command. The
        // picker takes single keys, not text, so it can spare this one — and
        // needs to, since its keys are the ones least likely to be guessed.
        if key.code == KeyCode::Char('?') && matches!(self.mode, AppMode::Normal | AppMode::Picker)
        {
            self.help_visible = true;
            return Ok(());
        }
        if self.screen == Screen::Browser {
            return self.handle_browser_key(key);
        }
        match self.mode {
            AppMode::Search => self.handle_search_key(key),
            AppMode::Edit => self.handle_edit_key(key),
            AppMode::Command => self.handle_command_key(key),
            AppMode::Picker => self.handle_picker_key(key),
            AppMode::Normal => self.handle_normal_key(key),
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.mode = AppMode::Normal;
                self.search_buf.clear();
            }
            KeyCode::Enter => {
                let raw = std::mem::take(&mut self.search_buf);
                self.mode = AppMode::Normal;
                if raw.is_empty() {
                    self.search_state = None;
                    self.search_rx = None;
                    return Ok(());
                }
                match SearchQuery::new(raw) {
                    Err(e) => {
                        self.message = Some(format!("Bad regex: {e}"));
                    }
                    Ok(query) => {
                        if let Some(store) = &self.store {
                            let col_name = match self.selection_mode {
                                SelectionMode::Column | SelectionMode::Cell => {
                                    store.column_info(self.cursor_col).map(|(name, _)| name)
                                }
                                SelectionMode::Row => None,
                            };
                            let (tx, rx) = mpsc::channel();
                            store.search_async(query.raw.clone(), col_name.clone(), tx);
                            // Replace any in-progress search (dropping old rx cancels it)
                            self.search_rx = Some(rx);
                            let col_idx = col_name.map(|_| self.cursor_col);
                            self.search_state = Some(SearchState::new(query, col_idx));
                        }
                    }
                }
            }
            KeyCode::Backspace => {
                self.search_buf.pop();
            }
            KeyCode::Char(c) => {
                self.search_buf.push(c);
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        if let Some(prefix) = self.pending_prefix.take() {
            return self.resolve_prefix(prefix, key.code);
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => self.quit(false),

            // '0' scrolls to the first column, leaving it at the left edge.
            // Must come before the digit-accumulation arm — but only when no
            // count is being typed, or `10j` would lose its zero.
            KeyCode::Char('0') if !ctrl && self.pending_num.is_empty() => {
                self.col_offset = 0;
                self.cursor_col = 0;
            }

            // Accumulate numeric prefix (used by j/k/G)
            KeyCode::Char(c) if c.is_ascii_digit() && !ctrl => {
                self.pending_num.push(c);
            }

            // Row navigation — move cursor; scroll only when cursor leaves the viewport
            KeyCode::Char('j') | KeyCode::Down => {
                let n = self.take_count(1);
                self.cursor_down(n)?;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let n = self.take_count(1);
                self.cursor_up(n)?;
            }
            KeyCode::Char('d') if ctrl => self.scroll_page(true, Page::Half)?,
            KeyCode::Char('u') if ctrl => self.scroll_page(false, Page::Half)?,
            KeyCode::Char('f') if ctrl => self.scroll_page(true, Page::Whole)?,
            KeyCode::Char('b') if ctrl => self.scroll_page(false, Page::Whole)?,
            KeyCode::PageDown => self.scroll_page(true, Page::Whole)?,
            KeyCode::PageUp => self.scroll_page(false, Page::Whole)?,

            // `g` and `d` wait for their second key. The count survives, so
            // `12gg` and `3dd` each read as one action.
            KeyCode::Char('g') => self.pending_prefix = Some('g'),
            // Over a selection `d` acts at once, on the rows already chosen,
            // so it does not wait for a second key.
            KeyCode::Char('d') if !ctrl && self.visual_anchor.is_none() => {
                self.pending_prefix = Some('d')
            }
            KeyCode::Home => {
                self.pending_num.clear();
                self.cursor_to(0)?;
            }
            KeyCode::Char('G') | KeyCode::End => {
                if self.pending_num.is_empty() {
                    let last = self
                        .store
                        .as_ref()
                        .map_or(0, |s| s.row_count().saturating_sub(1));
                    self.cursor_to(last)?;
                } else {
                    let s = std::mem::take(&mut self.pending_num);
                    self.jump_to_line(&s)?;
                }
            }

            // z-prefix: zz (centre), zt (top), zb (bottom), and the column
            // widths. `Z` opens it too: its second keys are shifted ones, and
            // the shift tends to go down before the `z` does.
            KeyCode::Char('z' | 'Z') => {
                self.pending_num.clear();
                self.pending_prefix = Some('z');
            }

            // Column navigation, counted the same way `j` and `k` are.
            KeyCode::Char('h') | KeyCode::Left => {
                let n = self.take_count(1);
                self.column_left(n);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                let n = self.take_count(1);
                self.column_right(n);
            }
            // Jump to first column (all modes) or last column (Column/Cell via $).
            KeyCode::Char('H') => {
                self.pending_num.clear();
                self.col_offset = 0;
                self.cursor_col = 0;
            }
            // '$' scrolls until the last column sits at the right edge.
            KeyCode::Char('$') => {
                self.pending_num.clear();
                self.cursor_col = self
                    .store
                    .as_ref()
                    .map_or(0, |s| s.column_count().saturating_sub(1));
                self.col_offset = self.max_col_offset();
            }

            // Sort by cursor column (Column/Cell mode only). Toggles asc ↔ desc;
            // pressing s on a new column adds it as the next priority sort key.
            // The picker needs no column cursor — it is a list of every
            // column, not an operation on the one under the cursor — so
            // unlike `-` it works in row mode too.
            KeyCode::Char('C') => {
                self.pending_num.clear();
                self.open_picker();
            }

            // Hide the cursor column. Gated to the cursor modes as `s` is:
            // row mode has no column cursor, and hiding whichever column
            // happens to be leftmost is not what the key means.
            KeyCode::Char('-')
                if matches!(
                    self.selection_mode,
                    SelectionMode::Column | SelectionMode::Cell
                ) =>
            {
                self.pending_num.clear();
                self.hide_column()?;
            }

            KeyCode::Char('s')
                if matches!(
                    self.selection_mode,
                    SelectionMode::Column | SelectionMode::Cell
                ) =>
            {
                self.visual_anchor = None;
                if let Some(reason) = self.store.as_ref().and_then(Store::sort_blocked) {
                    self.message = Some(reason);
                    return Ok(());
                }
                if let Some(store) = &mut self.store {
                    self.sort_rx = Some(store.begin_sort(self.cursor_col));
                    self.cursor_row = 0;
                    // Sort changes the frame order, so any active search positions
                    // are now stale. Clear the search.
                    self.search_state = None;
                    self.search_rx = None;
                }
            }

            // Cycle selection mode: Row → Column → Cell → Row
            KeyCode::Tab => {
                self.visual_anchor = None;
                self.selection_mode = self.selection_mode.cycle();
                if !matches!(self.selection_mode, SelectionMode::Row) {
                    // Start column cursor at the leftmost visible column.
                    self.cursor_col = self.col_offset;
                }
                // Clear search — scope has changed
                self.search_state = None;
                self.search_rx = None;
            }

            // Lake: jump to this table's file pane / back to the browser.
            KeyCode::Char('f') if self.lake.is_some() => {
                self.pending_num.clear();
                if let Some(scope) = self.lake.as_ref().and_then(|l| l.scope) {
                    self.show_partitions(scope.table, scope.partition.unwrap_or(0))?;
                }
            }
            KeyCode::Char('b') if self.lake.is_some() => {
                self.pending_num.clear();
                self.screen = Screen::Browser;
            }
            KeyCode::Char('T') if self.lake.is_some() => {
                self.pending_num.clear();
                if let Some(lake) = &mut self.lake {
                    lake.level = Level::Snapshots;
                    lake.state = Default::default();
                    let current = lake.current_snapshot_index();
                    lake.state.go_to(current, lake.list_len());
                    self.screen = Screen::Browser;
                }
            }

            // Search
            KeyCode::Char('/') => {
                self.pending_num.clear();
                self.search_buf.clear();
                self.mode = AppMode::Search;
            }
            KeyCode::Char('n') => {
                self.pending_num.clear();
                if let Some(state) = &mut self.search_state {
                    if let Some(ci) = state.col_idx {
                        self.cursor_col = ci;
                    }
                    if let Some(row) = state.next_from(self.cursor_row) {
                        self.cursor_to(row)?;
                    }
                }
            }
            KeyCode::Char('N') => {
                self.pending_num.clear();
                if let Some(state) = &mut self.search_state {
                    if let Some(ci) = state.col_idx {
                        self.cursor_col = ci;
                    }
                    if let Some(row) = state.prev_from(self.cursor_row) {
                        self.cursor_to(row)?;
                    }
                }
            }
            // Editing. Ctrl+u and Ctrl+d are matched further up, so plain 'u'
            // reaching here is always undo.
            KeyCode::Char('v') => {
                self.pending_num.clear();
                self.visual_anchor = match self.visual_anchor {
                    Some(_) => None,
                    None => Some((self.cursor_row, self.cursor_col)),
                };
            }
            KeyCode::Char('c') if self.visual_anchor.is_some() => {
                self.begin_fill(FillMode::Replace)?
            }
            KeyCode::Char('i') if self.visual_anchor.is_some() => {
                self.begin_fill(FillMode::Prepend)?
            }
            KeyCode::Char('a') if self.visual_anchor.is_some() => {
                self.begin_fill(FillMode::Append)?
            }
            KeyCode::Char('x') if self.visual_anchor.is_some() => self.clear_selection()?,
            KeyCode::Char('d') if self.visual_anchor.is_some() => {
                let rows = self.visual_range().map(|((first, last), _)| (first, last));
                if let Some((first, last)) = rows {
                    self.delete_rows(first, last - first + 1)?;
                }
            }
            // `K` is vim's "tell me about the thing under the cursor", and
            // that is what this is.
            KeyCode::Char('K') => {
                self.pending_num.clear();
                if matches!(self.selection_mode, SelectionMode::Row) {
                    self.selection_mode = SelectionMode::Cell;
                    self.cursor_col = self.col_offset;
                }
                self.cell_view = !self.cell_view;
            }
            KeyCode::Char('#') => {
                self.pending_num.clear();
                self.relative_rows = !self.relative_rows;
            }
            KeyCode::Char('o') => self.open_row(true)?,
            KeyCode::Char('O') => self.open_row(false)?,
            KeyCode::Char('y') => self.yank()?,
            KeyCode::Char('p') => self.paste()?,
            KeyCode::Char('i') => self.begin_edit(EditStart::Front)?,
            KeyCode::Char('a') => self.begin_edit(EditStart::End)?,
            KeyCode::Char('c') => self.begin_edit(EditStart::Empty)?,
            KeyCode::Char('x') => self.clear_cell()?,
            KeyCode::Char('u') => self.undo()?,
            KeyCode::Char('r') if ctrl => self.redo()?,
            KeyCode::Char(':') => {
                self.pending_num.clear();
                self.command_buf.clear();
                self.mode = AppMode::Command;
            }

            KeyCode::Esc if self.cell_view => {
                self.pending_num.clear();
                self.cell_view = false;
            }
            KeyCode::Esc if self.visual_anchor.is_some() => {
                self.pending_num.clear();
                self.visual_anchor = None;
            }
            KeyCode::Esc => {
                self.search_state = None;
                self.search_rx = None; // dropping rx cancels background scan
                self.pending_num.clear();
                if let Some(store) = &mut self.store
                    && !store.view.sort.is_empty()
                {
                    let _ = store.clear_sort();
                    self.cursor_row = 0;
                }
            }

            _ => {
                self.pending_num.clear();
            }
        }
        Ok(())
    }

    // ── editing ───────────────────────────────────────────────────────────

    /// Whether an edit can start here, reporting why not when it cannot.
    ///
    /// An edit needs a column cursor, so row mode adopts one rather than doing
    /// nothing — the leftmost visible column, which is where Tab would put it.
    fn ready_to_edit(&mut self) -> bool {
        let blocked = match &self.store {
            None => Some("no file open"),
            Some(store) => store.edit_blocked(),
        };
        if let Some(reason) = blocked {
            self.message = Some(reason.to_string());
            return false;
        }
        // Row mode has no column cursor, so a single-cell edit adopts one —
        // but a row-shaped *selection* is meaningful in its own right and must
        // not be reshaped out from under the operator about to run on it.
        if matches!(self.selection_mode, SelectionMode::Row) && self.visual_anchor.is_none() {
            self.selection_mode = SelectionMode::Cell;
            self.cursor_col = self.col_offset;
        }
        true
    }

    fn begin_edit(&mut self, start: EditStart) -> anyhow::Result<()> {
        self.pending_num.clear();
        if !self.ready_to_edit() {
            return Ok(());
        }
        let cell = (self.cursor_row, self.cursor_col);
        let Some(current) = self
            .store
            .as_ref()
            .and_then(|s| s.cell_text(cell.0, cell.1))
        else {
            return Ok(());
        };
        self.edit_buf = match start {
            EditStart::Empty => String::new(),
            EditStart::Front | EditStart::End => current,
        };
        self.edit_cursor = match start {
            EditStart::Front => 0,
            EditStart::End | EditStart::Empty => self.edit_buf.chars().count(),
        };
        self.edit_cell = Some(cell);
        self.mode = AppMode::Edit;
        Ok(())
    }

    /// `x`: empty the cell under the cursor without opening the editor.
    fn clear_cell(&mut self) -> anyhow::Result<()> {
        self.pending_num.clear();
        if !self.ready_to_edit() {
            return Ok(());
        }
        let cell = (self.cursor_row, self.cursor_col);
        if let Some(store) = &mut self.store {
            store.edit([(cell, String::new())])?;
        }
        Ok(())
    }

    /// The selected block, in absolute coordinates.
    fn visual_range(&self) -> Option<Block> {
        self.block_from(self.visual_anchor?)
    }

    /// The block an operator acts on with no selection: the cursor, shaped
    /// exactly as `v` would shape a selection of one.
    fn cursor_block(&self) -> Option<Block> {
        self.block_from((self.cursor_row, self.cursor_col))
    }

    /// A block spanning `anchor` to the cursor. Its shape follows the
    /// selection mode: whole rows, whole columns, or a rectangle.
    fn block_from(&self, (anchor_row, anchor_col): (usize, usize)) -> Option<Block> {
        let store = self.store.as_ref()?;
        let rows = (
            anchor_row.min(self.cursor_row),
            anchor_row.max(self.cursor_row),
        );
        let cols = (
            anchor_col.min(self.cursor_col),
            anchor_col.max(self.cursor_col),
        );
        let all_rows = (0, store.row_count().saturating_sub(1));
        let all_cols = (0, store.column_count().saturating_sub(1));
        Some(match self.selection_mode {
            SelectionMode::Row => (rows, all_cols),
            SelectionMode::Column => (all_rows, cols),
            SelectionMode::Cell => (rows, cols),
        })
    }

    fn block_size(((r0, r1), (c0, c1)): Block) -> usize {
        (r1 - r0 + 1).saturating_mul(c1 - c0 + 1)
    }

    /// Every cell in a block, row by row.
    fn cells_in(((r0, r1), (c0, c1)): Block) -> impl Iterator<Item = (usize, usize)> {
        (r0..=r1).flat_map(move |row| (c0..=c1).map(move |col| (row, col)))
    }

    /// Whether plv will take on a block of this size, complaining if not.
    fn within_limit(&mut self, block: Block) -> bool {
        let size = Self::block_size(block);
        if size > MAX_BLOCK {
            self.message = Some(format!(
                "{size} cells is too many to handle at once (limit {MAX_BLOCK})"
            ));
            return false;
        }
        true
    }

    /// The selected block an editing operator is about to run on, once it is
    /// known to be one the edit buffer can hold.
    fn operable_range(&mut self) -> Option<Block> {
        self.pending_num.clear();
        if !self.ready_to_edit() {
            return None;
        }
        let block = self.visual_range()?;
        self.within_limit(block).then_some(block)
    }

    /// `y`: copy the selection — or the cursor, shaped by the mode — into the
    /// register. Reading only, so it works on views that refuse edits.
    fn yank(&mut self) -> anyhow::Result<()> {
        self.pending_num.clear();
        let block = match self.visual_anchor {
            Some(_) => self.visual_range(),
            None => self.cursor_block(),
        };
        let Some(block) = block.filter(|&b| self.within_limit(b)) else {
            return Ok(());
        };
        let Some(store) = self.store.as_ref() else {
            return Ok(());
        };
        self.register = store.block_text(block.0, block.1)?;

        let cells = Self::block_size(block);
        // As in vim, a visual yank leaves the cursor where the selection
        // began, so a paste can be aimed from a known place rather than from
        // wherever the selection happened to end.
        if let Some((row, col)) = self.visual_anchor.take() {
            self.cursor_col = col;
            self.cursor_to(row)?;
        }
        self.message = Some(format!("yanked {cells} cells"));
        Ok(())
    }

    /// `p`: write the register into the grid with its top-left at the cursor.
    ///
    /// Overwrites rather than inserting — a fixed grid has nowhere to push
    /// cells along to — and anything past the last row or column is dropped
    /// rather than silently wrapping.
    fn paste(&mut self) -> anyhow::Result<()> {
        self.pending_num.clear();
        if self.register.is_empty() {
            self.message = Some("nothing to paste".to_string());
            return Ok(());
        }
        if !self.ready_to_edit() {
            return Ok(());
        }
        let Some(store) = self.store.as_ref() else {
            return Ok(());
        };
        let last_row = store.row_count().saturating_sub(1);
        let last_col = store.column_count().saturating_sub(1);

        let mut edits = Vec::new();
        let mut clipped = 0usize;
        for (down, row) in self.register.iter().enumerate() {
            for (across, value) in row.iter().enumerate() {
                let cell = (self.cursor_row + down, self.cursor_col + across);
                if cell.0 > last_row || cell.1 > last_col {
                    clipped += 1;
                } else {
                    edits.push((cell, value.clone()));
                }
            }
        }

        let pasted = edits.len();
        if let Some(store) = &mut self.store {
            store.edit(edits)?;
        }
        self.visual_anchor = None;
        self.message = Some(match clipped {
            0 => format!("pasted {pasted} cells"),
            n => format!("pasted {pasted} cells, {n} past the edge"),
        });
        Ok(())
    }

    /// `c`, `i` and `a` over a selection: type one value, apply it to every
    /// cell in the block.
    fn begin_fill(&mut self, mode: FillMode) -> anyhow::Result<()> {
        let Some(block) = self.operable_range() else {
            return Ok(());
        };
        self.edit_buf.clear();
        self.edit_cursor = 0;
        self.edit_cell = Some((self.cursor_row, self.cursor_col));
        self.edit_fill = Some(Fill { block, mode });
        self.mode = AppMode::Edit;
        Ok(())
    }

    /// `x` / `d` over a selection: empty every cell in it, as one undo step.
    fn clear_selection(&mut self) -> anyhow::Result<()> {
        let Some(block) = self.operable_range() else {
            return Ok(());
        };
        let edits: Vec<_> = Self::cells_in(block)
            .map(|cell| (cell, String::new()))
            .collect();
        let cleared = edits.len();
        if let Some(store) = &mut self.store {
            store.edit(edits)?;
        }
        self.visual_anchor = None;
        self.message = Some(format!("cleared {cleared} cells"));
        Ok(())
    }

    /// Strike out `count` rows from display position `first`.
    ///
    /// Nothing is removed from the file until `:w`; until then the rows are
    /// simply not shown, and `u` puts them back.
    fn delete_rows(&mut self, first: usize, count: usize) -> anyhow::Result<()> {
        self.pending_num.clear();
        let blocked = match &self.store {
            None => Some("no file open"),
            Some(store) => store.delete_blocked(),
        };
        if let Some(reason) = blocked {
            self.message = Some(reason.to_string());
            return Ok(());
        }

        let deleted = match &mut self.store {
            Some(store) => store.delete_rows(first..first + count)?,
            None => 0,
        };
        self.visual_anchor = None;

        // Everything below has moved up; the cursor may now be past the end.
        let last = self
            .store
            .as_ref()
            .map_or(0, |s| s.row_count().saturating_sub(1));
        if self.cursor_row > last {
            self.cursor_to(last)?;
        }
        self.message = Some(format!(
            "deleted {deleted} row{}",
            if deleted == 1 { "" } else { "s" }
        ));
        Ok(())
    }

    /// `o` and `O`: a new row below or above, ready to be typed into.
    ///
    /// Opens the editor on it straight away, as vim does — an empty row is
    /// only useful once something is in it.
    fn open_row(&mut self, below: bool) -> anyhow::Result<()> {
        self.pending_num.clear();
        let blocked = match &self.store {
            None => Some("no file open"),
            Some(store) => store.insert_blocked(),
        };
        if let Some(reason) = blocked {
            self.message = Some(reason.to_string());
            return Ok(());
        }

        let at = self.cursor_row;
        let landed = match &mut self.store {
            Some(store) => store.add_row(at, below)?,
            None => at,
        };
        self.visual_anchor = None;
        self.cursor_to(landed)?;

        // A row needs a column cursor to be typed into, the way a cell edit
        // does, and starts at the first column.
        if matches!(self.selection_mode, SelectionMode::Row) {
            self.selection_mode = SelectionMode::Cell;
        }
        self.cursor_col = 0;
        self.col_offset = 0;
        self.begin_edit(EditStart::Empty)
    }

    fn undo(&mut self) -> anyhow::Result<()> {
        self.pending_num.clear();
        let Some(store) = &mut self.store else {
            return Ok(());
        };
        if !store.undo()? {
            self.message = Some("nothing to undo".to_string());
        }
        Ok(())
    }

    fn redo(&mut self) -> anyhow::Result<()> {
        self.pending_num.clear();
        let Some(store) = &mut self.store else {
            return Ok(());
        };
        if !store.redo()? {
            self.message = Some("nothing to redo".to_string());
        }
        Ok(())
    }

    fn handle_edit_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.end_edit(),
            KeyCode::Enter => {
                self.commit_edit()?;
                self.end_edit();
            }
            // Commit and step sideways, so a row can be filled in without
            // leaving the editor between cells.
            KeyCode::Tab => self.commit_and_step(1)?,
            KeyCode::BackTab => self.commit_and_step(-1)?,
            KeyCode::Left => self.edit_cursor = self.edit_cursor.saturating_sub(1),
            KeyCode::Right => {
                self.edit_cursor = (self.edit_cursor + 1).min(self.edit_buf.chars().count());
            }
            KeyCode::Home => self.edit_cursor = 0,
            KeyCode::End => self.edit_cursor = self.edit_buf.chars().count(),
            KeyCode::Backspace => {
                if self.edit_cursor > 0 {
                    self.edit_cursor -= 1;
                    self.remove_edit_char(self.edit_cursor);
                }
            }
            KeyCode::Delete => self.remove_edit_char(self.edit_cursor),
            KeyCode::Char(c) if !ctrl => {
                let at = self.edit_byte_index(self.edit_cursor);
                self.edit_buf.insert(at, c);
                self.edit_cursor += 1;
            }
            _ => {}
        }
        Ok(())
    }

    /// Put the typed value in the edit buffer. Nothing reaches the file
    /// until `:w`.
    fn commit_edit(&mut self) -> anyhow::Result<()> {
        let Some(cell) = self.edit_cell else {
            return Ok(());
        };
        let value = std::mem::take(&mut self.edit_buf);

        let edits = match self.edit_fill {
            // A fill writes every cell in the block, including any that
            // already hold the value: comparing first would mean reading rows
            // that are not on the page.
            Some(fill) => self.fill_edits(fill, &value)?,
            None => {
                // Retyping what was already there should not mark the file
                // dirty, and should not rewrite the field: the bytes on disk
                // may be formatted differently from what Polars renders.
                let unchanged = self
                    .store
                    .as_ref()
                    .and_then(|s| s.cell_text(cell.0, cell.1))
                    .is_some_and(|current| current == value);
                if unchanged {
                    return Ok(());
                }
                vec![(cell, value)]
            }
        };

        // Read the warning off the values actually about to be written, which
        // for a prepend or an append is the only place they exist.
        let warning = edits
            .iter()
            .find_map(|((_, col), written)| self.type_warning(*col, written));
        let filled = edits.len();
        if let Some(store) = &mut self.store {
            store.edit(edits)?;
        }
        self.visual_anchor = None;
        self.message = warning.or_else(|| (filled > 1).then(|| format!("filled {filled} cells")));
        Ok(())
    }

    /// Every cell the block operator will write, paired with its new value.
    ///
    /// `Replace` needs nothing from the file. `Prepend` and `Append` build on
    /// what each cell already holds, so they read the block first — including
    /// the rows below the viewport, which is why the size is capped before the
    /// editor ever opens.
    fn fill_edits(&self, fill: Fill, typed: &str) -> anyhow::Result<Vec<((usize, usize), String)>> {
        if fill.mode == FillMode::Replace {
            return Ok(Self::cells_in(fill.block)
                .map(|cell| (cell, typed.to_string()))
                .collect());
        }
        let Some(store) = self.store.as_ref() else {
            return Ok(Vec::new());
        };
        let ((first_row, _), (first_col, _)) = fill.block;
        let current = store.block_text(fill.block.0, fill.block.1)?;

        Ok(Self::cells_in(fill.block)
            .map(|(row, col)| {
                let existing = current
                    .get(row - first_row)
                    .and_then(|cells| cells.get(col - first_col))
                    .map_or("", String::as_str);
                ((row, col), fill.mode.combine(typed, existing))
            })
            .collect())
    }

    /// Commit the current cell and open the next one along, if there is one.
    fn commit_and_step(&mut self, delta: isize) -> anyhow::Result<()> {
        if self.edit_fill.is_some() {
            // A fill has no "next cell" to step to; commit it and stop.
            self.commit_edit()?;
            self.end_edit();
            return Ok(());
        }
        self.commit_edit()?;
        let last = self
            .store
            .as_ref()
            .map_or(0, |s| s.column_count().saturating_sub(1));
        let next = self.cursor_col as isize + delta;
        self.end_edit();
        if next < 0 || next as usize > last {
            return Ok(());
        }
        self.cursor_col = next as usize;
        self.begin_edit(EditStart::End)
    }

    /// Warn when a value will not read back as its column's type.
    ///
    /// A warning and not a refusal: the file is text, the types are Polars'
    /// inference over it, and the user may well mean it. But the column will
    /// come back as text next time the file is opened, which is worth saying
    /// out loud.
    fn type_warning(&self, col: usize, value: &str) -> Option<String> {
        if value.is_empty() {
            return None; // an empty field is a null, which any column takes
        }
        let (name, dtype) = self.store.as_ref()?.column_info(col)?;
        let fits = match &dtype {
            d if d.is_integer() => value.parse::<i64>().is_ok(),
            d if d.is_float() => value.parse::<f64>().is_ok(),
            DataType::Boolean => matches!(value, "true" | "false"),
            _ => true,
        };
        (!fits).then(|| format!("\"{value}\" is not {dtype} — column \"{name}\" becomes text"))
    }

    fn end_edit(&mut self) {
        self.mode = AppMode::Normal;
        self.edit_cell = None;
        self.edit_fill = None;
        self.edit_buf.clear();
        self.edit_cursor = 0;
    }

    /// Byte offset of the `n`th character of the edit buffer, or its end.
    fn edit_byte_index(&self, n: usize) -> usize {
        self.edit_buf
            .char_indices()
            .nth(n)
            .map_or(self.edit_buf.len(), |(i, _)| i)
    }

    fn remove_edit_char(&mut self, at: usize) {
        if at < self.edit_buf.chars().count() {
            let i = self.edit_byte_index(at);
            self.edit_buf.remove(i);
        }
    }

    // ── ex commands ───────────────────────────────────────────────────────

    fn handle_command_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.mode = AppMode::Normal;
                self.command_buf.clear();
                self.forget_completion();
            }
            KeyCode::Enter => {
                let line = std::mem::take(&mut self.command_buf);
                self.mode = AppMode::Normal;
                self.forget_completion();
                self.run_command(line.trim())?;
            }
            KeyCode::Backspace => {
                self.forget_completion();
                if self.command_buf.pop().is_none() {
                    // Backspacing off the `:` leaves the command line, as in vim.
                    self.mode = AppMode::Normal;
                }
            }
            KeyCode::Tab => return self.complete_command(false),
            KeyCode::BackTab => return self.complete_command(true),
            KeyCode::Char(c) if !ctrl => {
                self.command_buf.push(c);
                self.forget_completion();
            }
            _ => {}
        }
        Ok(())
    }

    /// Tab on the command line: extend as far as the candidates agree, then
    /// step through them.
    ///
    /// Extending first is what makes it predictable — the line only ever grows
    /// towards something real. Stepping is what makes it useful once the
    /// common prefix has run out, which for column names is most of the time.
    fn complete_command(&mut self, backwards: bool) -> anyhow::Result<()> {
        if let Some(completion) = &self.completion
            && completion.options.len() > 1
        {
            let count = completion.options.len();
            let next = match self.completion_index {
                Some(index) if backwards => (index + count - 1) % count,
                Some(index) => (index + 1) % count,
                None if backwards => count - 1,
                None => 0,
            };
            self.command_buf = completion.with(next);
            self.completion_index = Some(next);
            return Ok(());
        }

        let Some(store) = &self.store else {
            return Ok(());
        };
        let Some(found) = complete::complete(&self.command_buf, &store.schema) else {
            self.forget_completion();
            return Ok(());
        };
        self.command_buf = found.extended();
        self.completion_index = None;
        // A single candidate has been applied in full; there is nothing left
        // to show or to step through.
        self.completion = (found.options.len() > 1).then_some(found);
        Ok(())
    }

    fn forget_completion(&mut self) {
        self.completion = None;
        self.completion_index = None;
    }

    /// `w` writes the buffer and `q` quits; a trailing `!` forces past the
    /// guard each of them puts up.
    fn run_command(&mut self, line: &str) -> anyhow::Result<()> {
        let (word, rest) = match line.split_once(char::is_whitespace) {
            Some((word, rest)) => (word, rest.trim()),
            None => (line, ""),
        };
        let force = word.ends_with('!');
        let path = (!rest.is_empty()).then(|| PathBuf::from(rest));

        match word.trim_end_matches('!') {
            "" => {}
            "w" => {
                self.write(path.as_deref(), force)?;
            }
            "wq" | "x" => {
                if self.write(path.as_deref(), force)? {
                    self.exit = true;
                }
            }
            "q" => self.quit(force),
            // Anything else is the view language, which reports its own
            // unknown-command error against the word that caused it.
            _ => self.run_view_command(line)?,
        }
        Ok(())
    }

    /// `:select`, `:hide`, `:filter`, `:sort`, `:reset`.
    ///
    /// Parsed and checked against the schema first, then tried on the frame:
    /// a view that will not collect is refused rather than adopted, so one
    /// mistyped command cannot leave the viewer showing an error.
    fn run_view_command(&mut self, line: &str) -> anyhow::Result<()> {
        let Some(store) = &self.store else {
            self.message = Some("no file open".to_string());
            return Ok(());
        };
        let command = match view::parse(line, &store.schema) {
            Ok(command) => command,
            Err(e) => {
                self.message = Some(e.message);
                return Ok(());
            }
        };
        self.apply_view_command(command)
    }

    /// Adopt a view command, whether it was typed on the `:` line or came
    /// from a key.
    ///
    /// Split out so `-` is genuinely the command it looks like rather than a
    /// second way to narrow the view: one path decides what a new view costs,
    /// refuses the ones that will not collect, and puts the cursor back.
    fn apply_view_command(&mut self, command: view::Command) -> anyhow::Result<()> {
        let Some(store) = &self.store else {
            self.message = Some("no file open".to_string());
            return Ok(());
        };

        // Whether the row set has to be rebuilt is a property of the command,
        // not of the state it produces: `:select` leaves a filter's matches
        // exactly as they were, and rescanning the file to rediscover that
        // would throw away the whole point of resolving it once.
        let refiltered = matches!(
            command,
            view::Command::Filter(_)
                | view::Command::Reset(None)
                | view::Command::Reset(Some(view::Slot::Filter))
        );

        // Asked before the key is recorded, so a refusal leaves the view as
        // it was rather than in an order nothing can produce.
        if let view::Command::Sort(keys) = &command
            && !keys.is_empty()
            && let Some(reason) = store.sort_blocked()
        {
            self.message = Some(reason);
            return Ok(());
        }

        let mut next = store.view.clone();
        let sort_before = next.sort.clone();
        if let Err(e) = next.apply(command, store.schema.len()) {
            self.message = Some(e);
            return Ok(());
        }
        let reordered = next.sort != sort_before;

        if let Some(store) = &mut self.store
            && let Err(e) = store.apply_view(next)
        {
            self.message = Some(e.to_string());
            return Ok(());
        }
        if reordered || refiltered {
            self.rebuild_view();
        }
        self.after_view_change(reordered || refiltered)?;
        Ok(())
    }

    /// Rebuild whatever backs the view, after a sort or a filter changed.
    ///
    /// The two are never both running. A held sort has the whole table in
    /// memory and applies the filter as part of building it, so a row-set scan
    /// would be re-reading a file that is already read. Without a sort the
    /// scan is the cheaper answer, because it never materialises anything.
    fn rebuild_view(&mut self) {
        // Dropping the receivers cancels whatever was still running.
        self.sort_rx = None;
        self.filter_rx = None;
        let Some(store) = &mut self.store else {
            return;
        };
        if store.view.sort.is_empty() {
            match store.begin_filter() {
                Ok(rx) => self.filter_rx = rx,
                Err(e) => self.message = Some(e.to_string()),
            }
        } else {
            self.sort_rx = Some(store.resort());
        }
    }

    /// Put the cursor back inside a view that may have fewer columns, and drop
    /// whatever was keyed to the old numbering.
    fn after_view_change(&mut self, reordered: bool) -> anyhow::Result<()> {
        let last = self
            .store
            .as_ref()
            .map_or(0, |s| s.column_count().saturating_sub(1));
        self.cursor_col = self.cursor_col.min(last);
        self.col_offset = self.col_offset.min(last);

        // A selection, and a search's match rows and scoped column, all refer
        // to the view that has just been replaced.
        self.visual_anchor = None;
        self.search_state = None;
        self.search_rx = None;

        if reordered {
            // The rows are in a different order, so the cursor's row number no
            // longer means what it did.
            self.cursor_to(0)?;
        }
        Ok(())
    }

    /// Returns whether the write happened; `:wq` only quits if it did.
    fn write(&mut self, dst: Option<&Path>, force: bool) -> anyhow::Result<bool> {
        let Some(store) = &mut self.store else {
            self.message = Some("no file open".to_string());
            return Ok(false);
        };
        match store.save(dst, force) {
            Ok(path) => {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file")
                    .to_string();
                self.message = Some(format!("wrote {name}"));
                Ok(true)
            }
            Err(e) => {
                self.message = Some(e.to_string());
                Ok(false)
            }
        }
    }

    /// Quitting refuses while the buffer holds unwritten edits.
    fn quit(&mut self, force: bool) {
        let dirty = self.store.as_ref().map_or(0, |s| s.dirty());
        if dirty > 0 && !force {
            self.message = Some(format!(
                "{dirty} unsaved edit{} — :w to write, :q! to discard",
                if dirty == 1 { "" } else { "s" }
            ));
            return;
        }
        self.exit = true;
    }

    /// The second key of a `g` or `z` sequence. Anything else cancels it.
    fn resolve_prefix(&mut self, prefix: char, code: KeyCode) -> anyhow::Result<()> {
        // The second key is read case-insensitively. Every `z` width command
        // takes a shifted key — `<`, `>`, `_` — and the shift naturally goes
        // down before the `z` does, so both keys arrive capitalised.
        let code = match code {
            KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
            other => other,
        };
        match (prefix, code) {
            ('z', KeyCode::Char('z')) => self.scroll_center(),
            ('z', KeyCode::Char('t')) => self.scroll_cursor_top(),
            ('z', KeyCode::Char('b')) => self.scroll_cursor_bottom(),
            // Column widths. `z` is where display adjustments live, here as in
            // vim, and a width is one: it changes how the table is drawn and
            // nothing about the data.
            ('z', KeyCode::Char('>')) => self.resize_column(1),
            ('z', KeyCode::Char('<')) => self.resize_column(-1),
            ('z', KeyCode::Char('_')) => self.fit_column(),
            ('z', KeyCode::Char('p')) => self.toggle_pin(),
            // Unpinning the lot is `z|` and not `zP`, because the second key
            // is lowercased above: a shifted letter cannot mean anything the
            // unshifted one does not. The divider is what a pin draws, so the
            // key names the thing it takes away.
            ('z', KeyCode::Char('|')) => {
                self.pinned.clear();
                Ok(())
            }
            ('z', KeyCode::Char('=')) => {
                self.widths.clear();
                self.message = Some("column widths reset".to_string());
                Ok(())
            }
            // `gg` is the first row, or the nth when a count precedes it.
            // `dd` takes out the cursor row, `{n}dd` that many.
            ('d', KeyCode::Char('d')) => {
                let count = self.take_count(1);
                self.delete_rows(self.cursor_row, count)
            }
            ('g', KeyCode::Char('g')) => {
                if self.pending_num.is_empty() {
                    self.cursor_to(0)
                } else {
                    let count = std::mem::take(&mut self.pending_num);
                    self.jump_to_line(&count)
                }
            }
            _ => {
                self.pending_num.clear();
                Ok(())
            }
        }
    }

    /// `Ctrl+d`/`Ctrl+u` and `Ctrl+f`/`Ctrl+b`: a screen or half of one, view
    /// and cursor together.
    ///
    /// vim moves both, keeping the cursor at the same height in the window,
    /// rather than walking the cursor down until it falls off the edge — so
    /// the view moves even when the cursor had room to spare. (The commands
    /// that scroll the view and leave the cursor behind are `Ctrl+e` and
    /// `Ctrl+y`, which plv does not have.)
    ///
    /// Against the ends of the file the view runs out of room first; the
    /// cursor then carries on alone, as it does in vim.
    fn scroll_page(&mut self, down: bool, page: Page) -> anyhow::Result<()> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let viewport = store.viewport_rows.max(1);
        let step = match page {
            Page::Half => (viewport / 2).max(1),
            Page::Whole => viewport.max(1),
        };
        let last_row = store.row_count().saturating_sub(1);
        let offset = store.row_offset;
        let height_in_view = self.cursor_row.saturating_sub(offset);

        let new_offset = if down {
            (offset + step).min(store.row_count().saturating_sub(viewport))
        } else {
            offset.saturating_sub(step)
        };

        if let Some(store) = &mut self.store {
            store.scroll_to_offset(new_offset)?;
        }
        let settled = self.store.as_ref().map_or(0, |s| s.row_offset);

        self.cursor_row = if settled == offset {
            // The view could not move, so only the cursor does.
            if down {
                (self.cursor_row + step).min(last_row)
            } else {
                self.cursor_row.saturating_sub(step)
            }
        } else {
            (settled + height_in_view).min(last_row)
        };
        Ok(())
    }

    // ── cursor + scroll helpers ────────────────────────────────────────────

    /// Move cursor to `row` (0-based), scrolling the viewport only if needed.
    fn cursor_to(&mut self, row: usize) -> anyhow::Result<()> {
        let (total, vp, offset) = match &self.store {
            Some(s) => (s.row_count(), s.viewport_rows, s.row_offset),
            None => return Ok(()),
        };
        if total == 0 {
            return Ok(());
        }
        let row = row.min(total - 1);
        self.cursor_row = row;

        let new_offset = if row < offset {
            row
        } else if row >= offset + vp {
            (row + 1).saturating_sub(vp)
        } else {
            return Ok(()); // already visible
        };

        if let Some(s) = &mut self.store {
            s.scroll_to_offset(new_offset)?;
        }
        Ok(())
    }

    fn cursor_down(&mut self, n: usize) -> anyhow::Result<()> {
        let total = self.store.as_ref().map_or(0, |s| s.row_count());
        if total == 0 {
            return Ok(());
        }
        let new_row = (self.cursor_row + n).min(total - 1);
        self.cursor_to(new_row)
    }

    fn cursor_up(&mut self, n: usize) -> anyhow::Result<()> {
        self.cursor_to(self.cursor_row.saturating_sub(n))
    }

    /// zz — scroll so the cursor is vertically centered.
    fn scroll_center(&mut self) -> anyhow::Result<()> {
        let (total, vp) = match &self.store {
            Some(s) => (s.row_count(), s.viewport_rows),
            None => return Ok(()),
        };
        let offset = self
            .cursor_row
            .saturating_sub(vp / 2)
            .min(total.saturating_sub(vp));
        if let Some(s) = &mut self.store {
            s.scroll_to_offset(offset)?;
        }
        Ok(())
    }

    /// zt — scroll so the cursor is at the top of the viewport.
    fn scroll_cursor_top(&mut self) -> anyhow::Result<()> {
        if let Some(s) = &mut self.store {
            s.scroll_to_offset(self.cursor_row)?;
        }
        Ok(())
    }

    /// zb — scroll so the cursor is at the bottom of the viewport.
    fn scroll_cursor_bottom(&mut self) -> anyhow::Result<()> {
        let (total, vp) = match &self.store {
            Some(s) => (s.row_count(), s.viewport_rows),
            None => return Ok(()),
        };
        let offset = self
            .cursor_row
            .saturating_sub(vp.saturating_sub(1))
            .min(total.saturating_sub(vp));
        if let Some(s) = &mut self.store {
            s.scroll_to_offset(offset)?;
        }
        Ok(())
    }

    /// Parse a buffered number string and jump to that 1-based line number.
    fn jump_to_line(&mut self, num_str: &str) -> anyhow::Result<()> {
        let total = self.store.as_ref().map_or(0, |s| s.row_count());
        match num_str.parse::<usize>() {
            Ok(0) => {
                self.message = Some(format!("Invalid line: 0  (valid: 1–{total})"));
            }
            Ok(n) if n > total => {
                self.message = Some(format!("Line {n} out of range  (file has {total} rows)"));
            }
            Ok(n) => self.cursor_to(n - 1)?,
            Err(_) => {
                self.message = Some(format!("Not a valid line number: \"{num_str}\""));
            }
        }
        Ok(())
    }

    /// Consume the pending numeric prefix, returning `default` if empty.
    fn take_count(&mut self, default: usize) -> usize {
        if self.pending_num.is_empty() {
            default
        } else {
            let n = self.pending_num.parse::<usize>().unwrap_or(default);
            self.pending_num.clear();
            n.max(1)
        }
    }

    /// Compute the `col_offset` that places `cursor_col` at the rightmost
    /// visible position in the current terminal width.
    ///
    /// Mirrors the Phase-1 width logic in `DataTable::render` (no redistribution
    /// needed — only visibility matters here).
    fn col_offset_to_show_at_right(&self, cursor_col: usize) -> usize {
        match &self.store {
            Some(store) => ui::col_offset_showing(
                &store.current_view,
                store.row_offset,
                self.last_frame_width,
                cursor_col,
                &self.widths,
                &self.display_pins(),
            ),
            None => 0,
        }
    }

    /// `h`: `n` columns left, or `n` columns of scroll in row mode.
    fn column_left(&mut self, n: usize) {
        match self.selection_mode {
            SelectionMode::Row => self.col_offset = self.col_offset.saturating_sub(n),
            SelectionMode::Column | SelectionMode::Cell => {
                self.cursor_col = self.cursor_col.saturating_sub(n);
                if !self.display_pins().contains(&self.cursor_col) {
                    self.col_offset = self.col_offset.min(self.cursor_col);
                }
            }
        }
    }

    /// `l`: `n` columns right, stopping at the last column — or, in row mode,
    /// where the last column reaches the right edge.
    ///
    /// The cursor modes only set the cursor; `draw` brings `col_offset` along
    /// if that has taken it out of view. Nudging the offset by hand here was
    /// only ever right for a single step.
    fn column_right(&mut self, n: usize) {
        match self.selection_mode {
            SelectionMode::Row => {
                self.col_offset = (self.col_offset + n).min(self.max_col_offset());
            }
            SelectionMode::Column | SelectionMode::Cell => {
                let last = self
                    .store
                    .as_ref()
                    .map_or(0, |s| s.column_count().saturating_sub(1));
                self.cursor_col = (self.cursor_col + n).min(last);
            }
        }
    }

    // ── the column picker ─────────────────────────────────────────────────

    /// `C`: open the list of every column, ticked for shown and pinned.
    fn open_picker(&mut self) {
        let Some(store) = &self.store else {
            return;
        };
        let names: Vec<String> = store
            .schema
            .iter_names()
            .map(|name| name.to_string())
            .collect();
        let order = store.view.columns(names.len());
        self.picker = Some(Picker::new(names, order, self.pinned.clone()));
        self.mode = AppMode::Picker;
    }

    fn close_picker(&mut self) {
        self.picker = None;
        self.mode = AppMode::Normal;
    }

    fn handle_picker_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Read before the picker is borrowed: a half page is a fact about the
        // screen, which the picker does not know.
        let half = self.last_vp as isize / 2;
        let Some(picker) = &mut self.picker else {
            self.mode = AppMode::Normal;
            return Ok(());
        };
        let len = picker.len();

        match key.code {
            // `q` is deliberately not bound. Everywhere else in vim it closes
            // a window, and a window is a view — closing one never destroys
            // work. The picker holds unapplied changes, so it cannot close
            // harmlessly, and borrowing the letter for something that throws
            // twenty toggles away is what makes `q` mean two things. `Esc`
            // discards, as it discards a half-typed `:` line.
            KeyCode::Esc => self.close_picker(),
            KeyCode::Enter => return self.apply_picker(),
            KeyCode::Char('j') | KeyCode::Down => picker.state.move_by(1, len),
            KeyCode::Char('k') | KeyCode::Up => picker.state.move_by(-1, len),
            KeyCode::Char('d') if ctrl => picker.state.move_by(half, len),
            KeyCode::Char('u') if ctrl => picker.state.move_by(-half, len),
            KeyCode::Char('g') | KeyCode::Home => picker.state.go_to(0, len),
            KeyCode::Char('G') | KeyCode::End => picker.state.go_to(len.saturating_sub(1), len),
            // `-` and not Space, so the key that takes a column off the view
            // is the same one in here as it is out there. In the table it can
            // only hide, since there is nothing on screen to un-hide; in the
            // list the state is in front of you, so it toggles.
            KeyCode::Char('-') => {
                if let Err(refusal) = picker.toggle_shown() {
                    self.message = Some(refusal);
                }
            }
            KeyCode::Char('p') => picker.toggle_pinned(),
            // Bulk over the whole list. Picking four columns out of two
            // hundred means starting from none, and unticking 196 by hand is
            // not a thing anyone will do.
            KeyCode::Char('a') => picker.show_all(),
            KeyCode::Char('A') => picker.show_only_cursor(),
            _ => {}
        }
        Ok(())
    }

    /// `Enter`: adopt what the picker holds.
    ///
    /// The selection goes through `apply_view_command` like every other way of
    /// narrowing the view, so the picker is a way of *writing* a `:select`
    /// rather than a second mechanism that decides what shows.
    fn apply_picker(&mut self) -> anyhow::Result<()> {
        let Some(picker) = self.picker.take() else {
            self.mode = AppMode::Normal;
            return Ok(());
        };
        self.mode = AppMode::Normal;

        let select = picker.selection();
        if select.is_empty() {
            // `toggle_shown` will not let it get here, and `View::apply` would
            // refuse it as well. Belt and braces, because the alternative is a
            // view with nothing in it.
            self.message = Some("that would hide every column".to_string());
            return Ok(());
        }
        self.apply_view_command(view::Command::Select(select))?;
        self.adopt_pins(picker.pins());
        Ok(())
    }

    /// Take the picker's pins, dropping any the screen has no room for.
    ///
    /// Trimmed rather than refused whole. The view change is what the user
    /// came for, and giving it up over a pin that does not fit would be
    /// abandoning the wrong half — so it keeps what fits and says what it
    /// could not take, the way a filter that fills its row set keeps what it
    /// has and reads `(first n)` rather than looking like the whole answer.
    ///
    /// Dropped from the right, because a pin is usually set on something that
    /// belongs at the left edge, and the leftmost is the one most likely meant.
    fn adopt_pins(&mut self, wanted: std::collections::BTreeSet<usize>) {
        self.pinned = wanted;
        let mut dropped = 0;
        while !self.pins_fit() {
            let Some(&rightmost) = self.display_pins().iter().next_back() else {
                break;
            };
            let Some(source) = self
                .store
                .as_ref()
                .and_then(|store| store.source_column(rightmost))
            else {
                break;
            };
            self.pinned.remove(&source);
            dropped += 1;
        }
        if dropped > 0 {
            self.message = Some(format!("no room for {dropped} of the pins"));
        }
    }

    /// Whether what is pinned still leaves room to scroll in.
    fn pins_fit(&self) -> bool {
        match &self.store {
            Some(store) => ui::pin_fits(
                &store.current_view,
                store.row_offset,
                self.last_frame_width,
                &self.widths,
                &self.display_pins(),
            ),
            None => true,
        }
    }

    /// `-`: take the cursor column off the view.
    ///
    /// Sugar for typing `:hide <name>`, and deliberately nothing more: it
    /// writes the slot `:select` writes, through the same `apply` that a
    /// typed line goes through, so there is one answer to which columns show
    /// rather than two that can disagree. `:hide` already reads the current
    /// list and filters it, so pressing this twice narrows twice — where a
    /// second `:select` would replace the first.
    ///
    /// The cursor is left where the column was, on whatever has moved into
    /// that position, as `dd` leaves it on the next row. That falls out of
    /// the clamp in `after_view_change` and needs nothing here.
    fn hide_column(&mut self) -> anyhow::Result<()> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let Some(source) = store.source_column(self.cursor_col) else {
            return Ok(());
        };
        let name = store
            .current_view
            .columns()
            .get(self.cursor_col)
            .map(|column| column.name().to_string());

        self.apply_view_command(view::Command::Hide(vec![source]))?;

        // Say how to get it back. There is no key that un-hides one column —
        // naming it is the only way to say which — so the way back has to be
        // in front of the user at the moment they might want it.
        if let Some(name) = name
            && self.message.is_none()
        {
            self.message = Some(format!("hid {name} — :reset select brings it back"));
        }
        Ok(())
    }

    /// Widen or narrow the cursor column by `steps`, each of a few characters.
    ///
    /// Columns after it are pushed along and off the right edge rather than
    /// squeezed, which is what a spreadsheet does and what `h`/`l` are for.
    fn resize_column(&mut self, steps: isize) -> anyhow::Result<()> {
        const STEP: isize = 4;
        let Some(source) = self
            .store
            .as_ref()
            .and_then(|store| store.source_column(self.cursor_col))
        else {
            return Ok(());
        };
        let current = match self.widths.get(&source) {
            Some(&width) => width as isize,
            None => self.drawn_width(source) as isize,
        };
        let want = (current + steps * STEP).max(ui::MIN_COLUMN as isize) as usize;
        self.widths.insert(source, want);
        Ok(())
    }

    /// Fit the cursor column to the widest value **on screen**.
    ///
    /// On screen and not in the file: the whole column is not in memory, and
    /// reading it to measure would be the full scan the rest of plv works to
    /// avoid.
    fn fit_column(&mut self) -> anyhow::Result<()> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let Some(source) = store.source_column(self.cursor_col) else {
            return Ok(());
        };
        let Some(column) = store.current_view.columns().get(self.cursor_col) else {
            return Ok(());
        };
        let widest = ui::natural_width(column);
        self.widths.insert(source, widest);
        Ok(())
    }

    /// The pins as display positions.
    ///
    /// `Store`'s indices are source columns and everything above it counts
    /// display positions, so the crossing happens here and the widget is
    /// handed a set it can use against the frame it is drawing. A pin on a
    /// column the current view hides simply is not in the set — it is not
    /// forgotten, it has nowhere to be drawn.
    fn display_pins(&self) -> ui::Pinned {
        let Some(store) = &self.store else {
            return ui::Pinned::new();
        };
        (0..store.column_count())
            .filter(|&display| {
                store
                    .source_column(display)
                    .is_some_and(|source| self.pinned.contains(&source))
            })
            .collect()
    }

    /// `zp`: hold the cursor column at the left edge, or let it go again.
    ///
    /// The use is comparison: a key or a label stays in sight while `h` and
    /// `l` walk the columns it is being read against, which on a wide table
    /// otherwise means scrolling back and forth and holding a value in your
    /// head.
    fn toggle_pin(&mut self) -> anyhow::Result<()> {
        let Some(source) = self
            .store
            .as_ref()
            .and_then(|store| store.source_column(self.cursor_col))
        else {
            return Ok(());
        };
        if self.pinned.remove(&source) {
            return Ok(());
        }

        // Set the pin, then ask the layout whether what it makes still leaves
        // room to scroll in, and take it back if not. Asking after rather than
        // predicting before means the check sees the block that would actually
        // be drawn, widths set by hand and all.
        self.pinned.insert(source);
        if !self.pins_fit() {
            self.pinned.remove(&source);
            self.message = Some("no room to pin: unpin one, or narrow one with z<".into());
        }
        Ok(())
    }

    /// What the cursor column is drawn at right now.
    fn drawn_width(&self, source: usize) -> usize {
        self.store
            .as_ref()
            .and_then(|store| store.current_view.columns().get(self.cursor_col))
            .map(|column| ui::drawn_width(column, source, self.last_frame_width, &self.widths))
            .unwrap_or(ui::MIN_COLUMN)
    }

    /// The column the status bar names.
    ///
    /// Row mode has no column cursor, so it reports the leftmost visible
    /// column — which is exactly what `h` and `l` move there. The other modes
    /// move a cursor, so the readout follows that instead. Either way the
    /// number moves when the keys that move sideways are pressed.
    fn col_position(&self) -> usize {
        match self.selection_mode {
            SelectionMode::Row => self.col_offset,
            _ => self.cursor_col,
        }
    }

    /// The furthest right the view can scroll: the offset that leaves the last
    /// column at the right edge. Going past it pads the view with empty space
    /// instead of data, which is what row mode used to do.
    fn max_col_offset(&self) -> usize {
        let last = self
            .store
            .as_ref()
            .map_or(0, |s| s.column_count().saturating_sub(1));
        self.col_offset_to_show_at_right(last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "name,count\na,1\nb,2\nc,3\n";

    fn app_with(name: &str, contents: &str) -> (App, PathBuf) {
        let dir = std::env::temp_dir().join("plv-app-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        let mut app = App::new(Some(path.clone()));
        app.store = Some(Store::open_file(&path, 10).unwrap());
        (app, path)
    }

    fn key(app: &mut App, event: impl IntoKeyEvent) {
        app.handle_key_event(event.into_key_event()).unwrap();
    }

    /// So a test can press a plain key or one with modifiers.
    trait IntoKeyEvent {
        fn into_key_event(self) -> KeyEvent;
    }
    impl IntoKeyEvent for KeyCode {
        fn into_key_event(self) -> KeyEvent {
            KeyEvent::new(self, KeyModifiers::NONE)
        }
    }
    impl IntoKeyEvent for KeyEvent {
        fn into_key_event(self) -> KeyEvent {
            self
        }
    }

    fn press(app: &mut App, c: char) {
        key(app, KeyCode::Char(c));
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, c);
        }
    }

    /// Run an ex command the way a user would: `:`, the text, Enter.
    fn command(app: &mut App, line: &str) {
        press(app, ':');
        typed(app, line);
        key(app, KeyCode::Enter);
    }

    fn shown(app: &App, col: usize, row: usize) -> Option<String> {
        app.store.as_ref()?.cell_text(row, col)
    }

    #[test]
    fn typing_a_cell_and_writing_it_reaches_the_file() {
        let (mut app, path) = app_with("edit.csv", SAMPLE);

        press(&mut app, 'i');
        assert!(matches!(app.mode, AppMode::Edit));
        // Row mode has no column cursor, so an edit adopts one.
        assert_eq!(app.selection_mode, SelectionMode::Cell);
        // `i` keeps the value it found.
        assert_eq!(app.edit_buf, "a");

        typed(&mut app, "bc");
        key(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, AppMode::Normal));
        // `i` inserts before the character the caret is on, as in vim.
        assert_eq!(shown(&app, 0, 0).as_deref(), Some("bca"));
        assert_eq!(app.store.as_ref().unwrap().dirty(), 1);
        // Nothing has touched the file yet.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SAMPLE);

        command(&mut app, "w");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "name,count\nbca,1\nb,2\nc,3\n"
        );
        assert_eq!(app.store.as_ref().unwrap().dirty(), 0);
    }

    #[test]
    fn i_and_a_put_the_caret_at_opposite_ends() {
        let (mut app, _) = app_with("caret.csv", SAMPLE);

        press(&mut app, 'i');
        assert_eq!(app.edit_cursor, 0);
        typed(&mut app, "x");
        key(&mut app, KeyCode::Enter);
        assert_eq!(shown(&app, 0, 0).as_deref(), Some("xa"));

        press(&mut app, 'a');
        assert_eq!(app.edit_buf, "xa");
        assert_eq!(app.edit_cursor, 2);
        typed(&mut app, "z");
        key(&mut app, KeyCode::Enter);
        assert_eq!(shown(&app, 0, 0).as_deref(), Some("xaz"));
    }

    #[test]
    fn c_replaces_the_value_and_escape_abandons_it() {
        let (mut app, _) = app_with("abandon.csv", SAMPLE);

        press(&mut app, 'c');
        assert_eq!(app.edit_buf, "", "c starts from an empty field");
        typed(&mut app, "gone");
        key(&mut app, KeyCode::Esc);

        assert!(matches!(app.mode, AppMode::Normal));
        assert_eq!(shown(&app, 0, 0).as_deref(), Some("a"));
        assert_eq!(app.store.as_ref().unwrap().dirty(), 0);
    }

    #[test]
    fn x_clears_a_cell_and_u_takes_it_back() {
        let (mut app, _) = app_with("clear.csv", SAMPLE);

        press(&mut app, 'x');
        assert_eq!(shown(&app, 0, 0).as_deref(), Some(""));
        assert_eq!(app.store.as_ref().unwrap().dirty(), 1);

        press(&mut app, 'u');
        assert_eq!(shown(&app, 0, 0).as_deref(), Some("a"));
        assert_eq!(app.store.as_ref().unwrap().dirty(), 0);

        press(&mut app, 'u');
        assert_eq!(app.message.as_deref(), Some("nothing to undo"));
    }

    #[test]
    fn retyping_the_same_value_is_not_an_edit() {
        let (mut app, _) = app_with("noop.csv", SAMPLE);

        press(&mut app, 'i');
        key(&mut app, KeyCode::Enter);
        assert_eq!(
            app.store.as_ref().unwrap().dirty(),
            0,
            "an unchanged cell must not dirty the file"
        );
    }

    #[test]
    fn a_value_that_breaks_the_column_type_warns_but_is_still_taken() {
        let (mut app, _) = app_with("warn.csv", SAMPLE);

        // Move to the numeric column, then replace its value with text.
        press(&mut app, 'i');
        key(&mut app, KeyCode::Esc);
        press(&mut app, 'l');
        press(&mut app, 'c');
        typed(&mut app, "n/a");
        key(&mut app, KeyCode::Enter);

        let message = app.message.clone().expect("a type mismatch should warn");
        assert!(message.contains("becomes text"), "{message}");
        assert_eq!(shown(&app, 1, 0).as_deref(), Some("n/a"));
    }

    #[test]
    fn tab_commits_and_moves_along_the_row() {
        let (mut app, _) = app_with("tab.csv", SAMPLE);

        press(&mut app, 'c');
        typed(&mut app, "first");
        key(&mut app, KeyCode::Tab);

        assert!(matches!(app.mode, AppMode::Edit), "still editing");
        assert_eq!(app.cursor_col, 1);
        assert_eq!(app.edit_buf, "1", "the next cell's value is loaded");

        typed(&mut app, "0");
        key(&mut app, KeyCode::Enter);
        assert_eq!(shown(&app, 0, 0).as_deref(), Some("first"));
        assert_eq!(shown(&app, 1, 0).as_deref(), Some("10"));
    }

    #[test]
    fn tab_off_the_last_column_leaves_the_editor() {
        let (mut app, _) = app_with("tabend.csv", SAMPLE);

        press(&mut app, 'i');
        key(&mut app, KeyCode::Esc);
        press(&mut app, 'l');
        press(&mut app, 'c');
        typed(&mut app, "9");
        key(&mut app, KeyCode::Tab);

        assert!(matches!(app.mode, AppMode::Normal));
        assert_eq!(shown(&app, 1, 0).as_deref(), Some("9"));
    }

    #[test]
    fn quitting_refuses_while_edits_are_unwritten() {
        let (mut app, _) = app_with("quit.csv", SAMPLE);
        press(&mut app, 'x');

        press(&mut app, 'q');
        assert!(!app.exit);
        let message = app.message.clone().unwrap();
        assert!(message.contains("1 unsaved edit"), "{message}");

        command(&mut app, "q");
        assert!(!app.exit, ":q is guarded too");

        command(&mut app, "q!");
        assert!(app.exit);
    }

    #[test]
    fn wq_writes_before_quitting_and_stays_put_when_the_write_fails() {
        let (mut app, path) = app_with("wq.csv", SAMPLE);
        press(&mut app, 'x');

        // Something else rewrites the file underneath the buffer.
        std::fs::write(&path, "name,count\nz,9\n").unwrap();
        command(&mut app, "wq");
        assert!(!app.exit, "a refused write must not quit");
        assert!(app.message.clone().unwrap().contains("changed on disk"));

        // Putting the original bytes back does not restore the timestamp, so
        // the buffer is still looking at a file it no longer recognises.
        std::fs::write(&path, SAMPLE).unwrap();
        command(&mut app, "wq");
        assert!(!app.exit, "same bytes, new mtime — still guarded");

        command(&mut app, "wq!");
        assert!(app.exit);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "name,count\n,1\nb,2\nc,3\n"
        );
    }

    /// Tab cycles Row → Column → Cell.
    fn cell_mode(app: &mut App) {
        key(app, KeyCode::Tab);
        key(app, KeyCode::Tab);
        assert_eq!(app.selection_mode, SelectionMode::Cell);
    }

    /// Eight narrow columns: more than fit, so scrolling has somewhere to go.
    const WIDE: &str = "a,b,c,d,e,f,g,h\n1,2,3,4,5,6,7,8\n";

    /// `last_frame_width` is normally set by `draw`, which tests do not run.
    fn app_sized(name: &str, contents: &str, width: u16) -> App {
        let (mut app, _) = app_with(name, contents);
        app.last_frame_width = width;
        app
    }

    #[test]
    fn row_mode_stops_where_the_last_column_reaches_the_right_edge() {
        let mut app = app_sized("wide.csv", WIDE, 40);
        assert_eq!(app.selection_mode, SelectionMode::Row);

        for _ in 0..20 {
            press(&mut app, 'l');
        }
        let last = 7;
        assert_eq!(app.col_offset, app.max_col_offset());
        assert!(
            app.col_offset < last,
            "row mode padded the view with empty columns: offset {} of {last}",
            app.col_offset
        );
    }

    #[test]
    fn dollar_and_zero_move_the_viewport_in_every_mode() {
        for enter_cell_mode in [false, true] {
            let mut app = app_sized("ends.csv", WIDE, 40);
            if enter_cell_mode {
                cell_mode(&mut app);
            }

            press(&mut app, '$');
            assert_eq!(
                app.col_offset,
                app.max_col_offset(),
                "cell={enter_cell_mode}"
            );
            assert_eq!(app.cursor_col, 7, "cell={enter_cell_mode}");

            press(&mut app, '0');
            assert_eq!(app.col_offset, 0, "cell={enter_cell_mode}");
            assert_eq!(app.cursor_col, 0, "cell={enter_cell_mode}");
        }
    }

    #[test]
    fn zero_stays_a_digit_while_a_count_is_being_typed() {
        let mut rows = String::from("a,b\n");
        for i in 0..20 {
            rows.push_str(&format!("{i},{i}\n"));
        }
        let mut app = app_sized("count.csv", &rows, 40);
        cell_mode(&mut app);

        press(&mut app, '1');
        press(&mut app, '0');
        assert_eq!(app.pending_num, "10", "'0' continued the count");
        press(&mut app, 'j');
        assert_eq!(app.cursor_row, 10);

        // On its own it is still the jump to the first column.
        press(&mut app, 'l');
        press(&mut app, '0');
        assert_eq!(app.cursor_col, 0);
    }

    #[test]
    fn h_and_l_take_a_count_like_j_and_k() {
        let mut app = app_sized("countcol.csv", WIDE, 40);
        cell_mode(&mut app);

        press(&mut app, '5');
        press(&mut app, 'l');
        assert_eq!(app.cursor_col, 5);
        assert!(app.pending_num.is_empty(), "the count is spent");

        press(&mut app, '3');
        press(&mut app, 'h');
        assert_eq!(app.cursor_col, 2);

        // Uncounted, they are still single steps.
        press(&mut app, 'l');
        assert_eq!(app.cursor_col, 3);
    }

    #[test]
    fn a_counted_column_jump_stops_at_the_ends() {
        let mut app = app_sized("countclamp.csv", WIDE, 40);
        cell_mode(&mut app);

        press(&mut app, '9');
        press(&mut app, '9');
        press(&mut app, 'l');
        assert_eq!(app.cursor_col, 7, "eight columns, so the last is 7");

        press(&mut app, '9');
        press(&mut app, '9');
        press(&mut app, 'h');
        assert_eq!(app.cursor_col, 0);
    }

    #[test]
    fn a_counted_jump_scrolls_row_mode_without_overshooting() {
        let mut app = app_sized("countrow.csv", WIDE, 40);
        assert_eq!(app.selection_mode, SelectionMode::Row);

        press(&mut app, '2');
        press(&mut app, 'l');
        assert_eq!(app.col_offset, 2);

        // Past the end it settles where the last column reaches the edge,
        // exactly where repeated single steps stop.
        press(&mut app, '9');
        press(&mut app, 'l');
        assert_eq!(app.col_offset, app.max_col_offset());
    }

    #[test]
    fn the_column_readout_follows_whatever_is_moving() {
        let mut app = app_sized("readout.csv", WIDE, 40);

        // Row mode moves the viewport, so the readout tracks that.
        press(&mut app, 'l');
        assert_eq!(app.col_offset, 1);
        assert_eq!(app.col_position(), 1);

        // Cell mode moves a cursor, so it tracks that instead — it used to
        // sit still while the cursor walked across the screen.
        cell_mode(&mut app);
        let before = app.col_position();
        press(&mut app, 'l');
        assert_eq!(app.col_position(), app.cursor_col);
        assert_ne!(app.col_position(), before);
    }

    /// 40 rows, so a 10-row viewport has plenty of room to move.
    fn tall_app(name: &str) -> App {
        let mut rows = String::from("n\n");
        for i in 0..40 {
            rows.push_str(&format!("{i}\n"));
        }
        let (mut app, _) = app_with(name, &rows);
        app.last_frame_width = 40;
        app
    }

    fn offset(app: &App) -> usize {
        app.store.as_ref().unwrap().row_offset
    }

    #[test]
    fn it_takes_two_gs_to_reach_the_top() {
        let mut app = tall_app("gg.csv");
        app.cursor_to(20).unwrap();

        press(&mut app, 'g');
        assert_eq!(app.cursor_row, 20, "one g is only half a motion");
        assert_eq!(app.pending_prefix, Some('g'));

        press(&mut app, 'g');
        assert_eq!(app.cursor_row, 0);
        assert_eq!(app.pending_prefix, None);
    }

    #[test]
    fn a_count_survives_the_first_g() {
        let mut app = tall_app("countgg.csv");
        press(&mut app, '1');
        press(&mut app, '2');
        press(&mut app, 'g');
        assert_eq!(app.pending_num, "12", "the count outlives the prefix");
        press(&mut app, 'g');
        assert_eq!(app.cursor_row, 11, "12gg is the twelfth row, 1-based");
    }

    #[test]
    fn an_unfinished_g_is_abandoned_rather_than_acted_on() {
        let mut app = tall_app("gcancel.csv");
        app.cursor_to(20).unwrap();

        press(&mut app, 'g');
        press(&mut app, 'x'); // not a g-command
        assert_eq!(app.pending_prefix, None);
        assert_eq!(app.cursor_row, 20);
        assert_eq!(app.store.as_ref().unwrap().dirty(), 0, "and x did not fire");
    }

    #[test]
    fn a_half_page_moves_the_view_even_when_the_cursor_has_room() {
        let mut app = tall_app("halfpage.csv");
        let viewport = app.store.as_ref().unwrap().viewport_rows;
        assert_eq!(viewport, 10);

        // Cursor at the very top of the window, with nine rows to spare.
        assert_eq!((app.cursor_row, offset(&app)), (0, 0));
        app.handle_key_event(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .unwrap();

        // vim scrolls the window and takes the cursor with it, keeping its
        // height in the window. Walking the cursor down alone would have left
        // the view untouched.
        assert_eq!(offset(&app), 5, "the view moved half a screen");
        assert_eq!(app.cursor_row, 5, "and the cursor kept its place in it");

        app.handle_key_event(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!((app.cursor_row, offset(&app)), (0, 0));
    }

    #[test]
    fn a_whole_page_moves_twice_as_far_as_half_of_one() {
        let mut app = tall_app("wholepage.csv");
        let viewport = app.store.as_ref().unwrap().viewport_rows;
        assert_eq!(viewport, 10);

        app.handle_key_event(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!(offset(&app), viewport, "a whole screen");
        assert_eq!(app.cursor_row, viewport, "the cursor came with it");

        app.handle_key_event(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!((offset(&app), app.cursor_row), (0, 0));

        // Page Up and Page Down say the same thing, as they do in csvlens.
        key(&mut app, KeyCode::PageDown);
        assert_eq!(offset(&app), viewport);
        key(&mut app, KeyCode::PageUp);
        assert_eq!(offset(&app), 0);
    }

    #[test]
    fn at_the_end_of_the_file_the_cursor_carries_on_alone() {
        let mut app = tall_app("halfend.csv");
        press(&mut app, 'G');
        let settled = offset(&app);
        assert_eq!(app.cursor_row, 39);

        app.handle_key_event(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!(offset(&app), settled, "the view has nowhere left to go");
        assert_eq!(app.cursor_row, 39, "and the cursor is already at the end");
    }

    #[test]
    fn z_commands_place_the_cursor_row_in_the_viewport() {
        let mut app = tall_app("zcmd.csv");
        assert_eq!(app.store.as_ref().unwrap().viewport_rows, 10);
        app.cursor_to(20).unwrap();

        press(&mut app, 'z');
        press(&mut app, 't');
        assert_eq!(offset(&app), 20, "zt puts the cursor at the top");

        press(&mut app, 'z');
        press(&mut app, 'z');
        assert_eq!(offset(&app), 15, "zz centres it");

        press(&mut app, 'z');
        press(&mut app, 'b');
        assert_eq!(offset(&app), 11, "zb puts it at the bottom");
        assert_eq!(app.cursor_row, 20, "and none of them move the cursor");
    }

    const FOURCOL: &str = "a,b,c,d\n1,2,3,4\n5,6,7,8\n";

    fn shown_columns(app: &App) -> Vec<String> {
        let store = app.store.as_ref().unwrap();
        store
            .current_view
            .get_column_names()
            .iter()
            .map(|n| n.to_string())
            .collect()
    }

    #[test]
    fn select_narrows_the_view_to_the_named_columns_in_order() {
        let mut app = app_sized("select.csv", FOURCOL, 60);
        command(&mut app, "select c a");
        assert_eq!(shown_columns(&app), ["c", "a"]);
        assert_eq!(app.store.as_ref().unwrap().column_count(), 2);

        // Each command replaces the slot rather than narrowing further.
        command(&mut app, "select b");
        assert_eq!(shown_columns(&app), ["b"]);

        command(&mut app, "reset");
        assert_eq!(shown_columns(&app), ["a", "b", "c", "d"]);
    }

    const CATS: &str = "id,cat\n0,a\n1,b\n2,a\n3,b\n4,a\n";

    /// The scan runs on a worker thread; drain it until it finishes.
    fn settle(app: &mut App) {
        for _ in 0..2000 {
            if app.filter_rx.is_none() {
                return;
            }
            app.poll_filter();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("the filter scan never finished");
    }

    fn filter(app: &mut App, line: &str) {
        command(app, line);
        settle(app);
    }

    #[test]
    fn a_filter_narrows_the_rows_on_show() {
        let mut app = app_sized("filter.csv", CATS, 60);
        filter(&mut app, "filter cat = a");

        let store = app.store.as_ref().unwrap();
        assert_eq!(store.row_count(), 3, "three rows say a");
        assert_eq!(store.total_rows, 5, "the file still has five");
        assert_eq!(shown(&app, 0, 0).as_deref(), Some("0"));
        assert_eq!(shown(&app, 0, 1).as_deref(), Some("2"));
        assert_eq!(shown(&app, 0, 2).as_deref(), Some("4"));
    }

    #[test]
    fn a_bare_filter_puts_the_rows_back() {
        let mut app = app_sized("unfilter.csv", CATS, 60);
        filter(&mut app, "filter cat = a");
        assert_eq!(app.store.as_ref().unwrap().row_count(), 3);

        filter(&mut app, "filter");
        assert_eq!(app.store.as_ref().unwrap().row_count(), 5);
        assert_eq!(shown(&app, 0, 1).as_deref(), Some("1"));
    }

    /// The reason a filter is resolved to row indices rather than composed
    /// into the frame: the edit buffer is keyed by source row, so a row picked
    /// out of a filtered view has to know which line of the file it came from.
    #[test]
    fn editing_a_filtered_row_writes_the_right_line() {
        let (mut app, path) = app_with("filteredit.csv", CATS);
        app.last_frame_width = 60;
        filter(&mut app, "filter cat = a");

        // Display row 1 is the file's row 2.
        press(&mut app, 'j');
        cell_mode(&mut app);
        press(&mut app, 'c');
        typed(&mut app, "99");
        key(&mut app, KeyCode::Enter);
        assert_eq!(shown(&app, 0, 1).as_deref(), Some("99"));

        command(&mut app, "w");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "id,cat\n0,a\n1,b\n99,a\n3,b\n4,a\n",
            "the edit belongs to the third line, not the second"
        );
    }

    #[test]
    fn a_filtered_view_can_still_be_edited() {
        let mut app = app_sized("filteditable.csv", CATS, 60);
        filter(&mut app, "filter cat = a");
        assert_eq!(
            app.store.as_ref().unwrap().edit_blocked(),
            None,
            "a filter keeps row identity, so it need not block edits"
        );
    }

    #[test]
    fn a_filter_and_a_sort_hold_at_the_same_time() {
        let mut app = app_sized("filtersort.csv", CATS, 60);
        filter(&mut app, "filter cat = a");
        assert_eq!(app.store.as_ref().unwrap().row_count(), 3);

        // Sorting a filtered view keeps the filter, and the `s` key agrees.
        command(&mut app, "sort id-");
        settle_sort(&mut app);
        let store = app.store.as_ref().unwrap();
        assert_eq!(store.row_count(), 3, "still only the rows that matched");
        assert_eq!(store.view.sort, [(0, false)]);
        assert!(store.view.filter.is_some());

        // Descending by id: 4, 2, 0 — the matching rows, reversed.
        assert_eq!(shown(&app, 0, 0).as_deref(), Some("4"));
        assert_eq!(shown(&app, 0, 1).as_deref(), Some("2"));
        assert_eq!(shown(&app, 0, 2).as_deref(), Some("0"));
        assert_eq!(store.edit_blocked(), None, "and it is still editable");
    }

    /// Both narrowings at once, and an edit through them still has to reach
    /// the right line of the file.
    #[test]
    fn editing_through_a_filtered_and_sorted_view_writes_the_right_line() {
        let (mut app, path) = app_with("filtersortedit.csv", CATS);
        app.last_frame_width = 60;
        filter(&mut app, "filter cat = a");
        command(&mut app, "sort id-");
        settle_sort(&mut app);

        // Display row 0 is id 4, the file's fifth row.
        cell_mode(&mut app);
        press(&mut app, 'c');
        typed(&mut app, "99");
        key(&mut app, KeyCode::Enter);
        assert_eq!(shown(&app, 0, 0).as_deref(), Some("99"));

        command(&mut app, "w");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "id,cat\n0,a\n1,b\n2,a\n3,b\n99,a\n"
        );
    }

    #[test]
    fn clearing_one_of_them_leaves_the_other_working() {
        let mut app = app_sized("filtersortclear.csv", CATS, 60);
        filter(&mut app, "filter cat = a");
        command(&mut app, "sort id-");
        settle_sort(&mut app);

        // Dropping the sort falls back to the row-set scan, filter intact.
        command(&mut app, "sort");
        settle(&mut app);
        let store = app.store.as_ref().unwrap();
        assert_eq!(store.row_count(), 3);
        assert_eq!(shown(&app, 0, 0).as_deref(), Some("0"), "file order again");

        // Dropping the filter leaves the whole file.
        command(&mut app, "filter");
        settle(&mut app);
        assert_eq!(app.store.as_ref().unwrap().row_count(), 5);
    }

    #[test]
    fn a_filter_that_matches_nothing_shows_nothing() {
        let mut app = app_sized("nomatch.csv", CATS, 60);
        filter(&mut app, "filter cat = zzz");
        let store = app.store.as_ref().unwrap();
        assert_eq!(store.row_count(), 0);
        assert_eq!(store.current_view.height(), 0);
        assert_eq!(
            store.current_view.width(),
            2,
            "still the right columns to draw a header from"
        );
    }

    #[test]
    fn a_filter_composes_with_a_projection() {
        let mut app = app_sized("filterselect.csv", CATS, 60);
        filter(&mut app, "filter cat = a");
        command(&mut app, "select cat");
        assert_eq!(shown_columns(&app), ["cat"]);
        assert_eq!(app.store.as_ref().unwrap().row_count(), 3);
        assert_eq!(shown(&app, 0, 0).as_deref(), Some("a"));
    }

    fn tab(app: &mut App) {
        key(app, KeyCode::Tab);
    }

    #[test]
    fn tab_completes_a_verb_then_a_column() {
        let mut app = app_sized("tabcomplete.csv", FOURCOL, 60);
        press(&mut app, ':');
        typed(&mut app, "sel");
        tab(&mut app);
        assert_eq!(app.command_buf, "select ");
        assert!(app.completion.is_none(), "one candidate needs no panel");

        typed(&mut app, "c");
        tab(&mut app);
        assert_eq!(app.command_buf, "select c ");
    }

    #[test]
    fn several_candidates_open_the_panel_and_tab_steps_through_them() {
        let mut app = app_sized("tabcycle.csv", "alpha,beta,gamma\n1,2,3\n", 60);
        press(&mut app, ':');
        typed(&mut app, "select ");

        tab(&mut app);
        let options = app.completion.as_ref().expect("a panel").options.clone();
        assert_eq!(options, ["alpha", "beta", "gamma"]);
        assert_eq!(app.panel_height(60, 24), 1, "the panel takes a row");

        // Nothing to extend — the three share no prefix — so Tab steps.
        assert_eq!(app.command_buf, "select ");
        tab(&mut app);
        assert_eq!(app.command_buf, "select alpha ");
        tab(&mut app);
        assert_eq!(app.command_buf, "select beta ");
        key(&mut app, KeyCode::BackTab);
        assert_eq!(app.command_buf, "select alpha ");
    }

    #[test]
    fn the_panel_takes_its_rows_from_the_table() {
        let mut app = app_sized("panelroom.csv", "alpha,beta,gamma\n1,2,3\n", 60);
        let before = App::viewport_rows(24, 0);
        press(&mut app, ':');
        typed(&mut app, "select ");
        tab(&mut app);
        let after = App::viewport_rows(24, app.panel_height(60, 24));
        assert_eq!(
            after,
            before - 1,
            "the table gives up exactly the panel's row"
        );
    }

    #[test]
    fn typing_or_leaving_puts_the_panel_away() {
        let mut app = app_sized("panelgone.csv", "alpha,beta,gamma\n1,2,3\n", 60);
        press(&mut app, ':');
        typed(&mut app, "select ");
        tab(&mut app);
        assert!(app.completion.is_some());

        typed(&mut app, "a");
        assert!(app.completion.is_none(), "a keystroke changes the answer");

        tab(&mut app);
        assert_eq!(app.command_buf, "select alpha ");
        key(&mut app, KeyCode::Esc);
        assert!(app.completion.is_none());
        assert_eq!(app.panel_height(60, 24), 0);
    }

    #[test]
    fn a_completed_command_actually_runs() {
        let mut app = app_sized("tabrun.csv", FOURCOL, 60);
        press(&mut app, ':');
        typed(&mut app, "sel");
        tab(&mut app);
        typed(&mut app, "d");
        tab(&mut app);
        key(&mut app, KeyCode::Enter);
        assert_eq!(shown_columns(&app), ["d"], "msg: {:?}", app.message);
    }

    #[test]
    fn dd_takes_out_the_cursor_row_and_writes_it_out() {
        let (mut app, path) = app_with("dd.csv", SAMPLE);
        app.last_frame_width = 60;
        press(&mut app, 'j'); // on row 1, `b`

        press(&mut app, 'd');
        assert_eq!(app.pending_prefix, Some('d'), "one d is half a command");
        assert_eq!(shown(&app, 0, 1).as_deref(), Some("b"), "nothing yet");

        press(&mut app, 'd');
        assert_eq!(app.store.as_ref().unwrap().row_count(), 2);
        assert_eq!(shown(&app, 0, 0).as_deref(), Some("a"));
        assert_eq!(shown(&app, 0, 1).as_deref(), Some("c"), "c moved up");

        // Nothing has reached the file until :w.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SAMPLE);
        command(&mut app, "w");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "name,count\na,1\nc,3\n"
        );
    }

    #[test]
    fn a_count_deletes_that_many_rows() {
        let (mut app, path) = app_with("countdd.csv", SAMPLE);
        app.last_frame_width = 60;
        press(&mut app, '2');
        press(&mut app, 'd');
        assert_eq!(app.pending_num, "2", "the count outlives the prefix");
        press(&mut app, 'd');

        assert_eq!(app.store.as_ref().unwrap().row_count(), 1);
        assert_eq!(shown(&app, 0, 0).as_deref(), Some("c"));
        command(&mut app, "w");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "name,count\nc,3\n");
    }

    #[test]
    fn deleting_is_one_undoable_step() {
        let (mut app, _) = app_with("undodd.csv", SAMPLE);
        app.last_frame_width = 60;
        press(&mut app, '2');
        press(&mut app, 'd');
        press(&mut app, 'd');
        assert_eq!(app.store.as_ref().unwrap().row_count(), 1);

        press(&mut app, 'u');
        assert_eq!(
            app.store.as_ref().unwrap().row_count(),
            3,
            "two rows, one undo"
        );
        assert_eq!(shown(&app, 0, 0).as_deref(), Some("a"));
        assert_eq!(app.store.as_ref().unwrap().dirty(), 0);
    }

    #[test]
    fn a_visual_selection_deletes_its_rows() {
        let (mut app, path) = app_with("visualdd.csv", SAMPLE);
        app.last_frame_width = 60;
        press(&mut app, 'v');
        press(&mut app, 'j');
        press(&mut app, 'd');

        assert_eq!(app.store.as_ref().unwrap().row_count(), 1);
        assert!(app.visual_anchor.is_none(), "the selection is spent");
        command(&mut app, "w");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "name,count\nc,3\n");
    }

    #[test]
    fn deleting_past_the_end_takes_what_is_there_and_moves_the_cursor_back() {
        let (mut app, _) = app_with("ddend.csv", SAMPLE);
        app.last_frame_width = 60;
        press(&mut app, 'j');
        press(&mut app, '9');
        press(&mut app, 'd');
        press(&mut app, 'd');

        assert_eq!(app.store.as_ref().unwrap().row_count(), 1);
        assert_eq!(app.cursor_row, 0, "the cursor cannot sit past the end");
        assert!(app.message.clone().unwrap().contains("deleted 2 rows"));
    }

    #[test]
    fn an_edited_cell_below_a_deleted_row_still_belongs_to_its_own_line() {
        // The edit is keyed to the file, the deletion shifts what is on
        // screen, and only the file can say whether they agree.
        let (mut app, path) = app_with("ddshift.csv", SAMPLE);
        app.last_frame_width = 60;
        cell_mode(&mut app);
        press(&mut app, 'd');
        press(&mut app, 'd'); // delete `a`, so `b` is now display row 0

        press(&mut app, 'c');
        typed(&mut app, "B");
        key(&mut app, KeyCode::Enter);
        assert_eq!(shown(&app, 0, 0).as_deref(), Some("B"));

        command(&mut app, "w");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "name,count\nB,2\nc,3\n",
            "the edit belongs to b's line, not a's"
        );
    }

    #[test]
    fn a_sorted_or_filtered_view_says_why_it_will_not_delete() {
        let mut app = app_sized("ddblocked.csv", CATS, 60);
        filter(&mut app, "filter cat = a");
        press(&mut app, 'd');
        press(&mut app, 'd');
        let message = app.message.clone().unwrap();
        assert!(message.contains("filtered or sorted"), "{message}");
        assert_eq!(app.store.as_ref().unwrap().row_count(), 3, "untouched");
    }

    #[test]
    fn o_opens_a_row_below_ready_to_type_into() {
        let (mut app, path) = app_with("open.csv", SAMPLE);
        app.last_frame_width = 60;
        press(&mut app, 'o');

        assert!(matches!(app.mode, AppMode::Edit), "typing starts at once");
        assert_eq!(app.cursor_row, 1, "below the row it was on");
        assert_eq!(app.store.as_ref().unwrap().row_count(), 4);

        typed(&mut app, "new");
        key(&mut app, KeyCode::Enter);
        assert_eq!(shown(&app, 0, 1).as_deref(), Some("new"));
        assert_eq!(shown(&app, 0, 2).as_deref(), Some("b"), "b moved down");

        command(&mut app, "w");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "name,count\na,1\nnew,\nb,2\nc,3\n"
        );
    }

    #[test]
    fn shift_o_opens_a_row_above() {
        let (mut app, path) = app_with("openabove.csv", SAMPLE);
        app.last_frame_width = 60;
        press(&mut app, 'j');
        press(&mut app, 'O');
        assert_eq!(app.cursor_row, 1, "where the old row 1 was");
        typed(&mut app, "mid");
        key(&mut app, KeyCode::Enter);

        command(&mut app, "w");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "name,count\na,1\nmid,\nb,2\nc,3\n"
        );
    }

    #[test]
    fn a_row_opened_at_the_top_lands_above_everything() {
        let (mut app, path) = app_with("opentop.csv", SAMPLE);
        app.last_frame_width = 60;
        press(&mut app, 'O');
        typed(&mut app, "first");
        key(&mut app, KeyCode::Enter);
        assert_eq!(shown(&app, 0, 0).as_deref(), Some("first"));

        command(&mut app, "w");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "name,count\nfirst,\na,1\nb,2\nc,3\n",
            "under the header, above the first row"
        );
    }

    #[test]
    fn a_row_opened_at_the_end_lands_after_everything() {
        let (mut app, path) = app_with("openend.csv", SAMPLE);
        app.last_frame_width = 60;
        press(&mut app, 'G');
        press(&mut app, 'o');
        typed(&mut app, "last");
        key(&mut app, KeyCode::Enter);

        command(&mut app, "w");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "name,count\na,1\nb,2\nc,3\nlast,\n"
        );
    }

    /// `o` twice running has to give two rows in the order they were opened,
    /// which is the case that breaks if a new row is only ever appended to its
    /// anchor rather than placed within it.
    #[test]
    fn opening_below_a_new_row_keeps_them_in_order() {
        let (mut app, path) = app_with("openorder.csv", SAMPLE);
        app.last_frame_width = 60;
        press(&mut app, 'o');
        typed(&mut app, "one");
        key(&mut app, KeyCode::Enter);
        press(&mut app, 'o');
        typed(&mut app, "two");
        key(&mut app, KeyCode::Enter);

        assert_eq!(shown(&app, 0, 1).as_deref(), Some("one"));
        assert_eq!(shown(&app, 0, 2).as_deref(), Some("two"));
        command(&mut app, "w");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "name,count\na,1\none,\ntwo,\nb,2\nc,3\n"
        );
    }

    #[test]
    fn opening_a_row_is_one_undoable_step() {
        let (mut app, _) = app_with("openundo.csv", SAMPLE);
        app.last_frame_width = 60;
        press(&mut app, 'o');
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.store.as_ref().unwrap().row_count(), 4);

        press(&mut app, 'u');
        assert_eq!(app.store.as_ref().unwrap().row_count(), 3);
        assert_eq!(app.store.as_ref().unwrap().dirty(), 0);
    }

    #[test]
    fn a_new_row_can_be_deleted_again() {
        let (mut app, path) = app_with("openthendelete.csv", SAMPLE);
        app.last_frame_width = 60;
        press(&mut app, 'o');
        typed(&mut app, "gone");
        key(&mut app, KeyCode::Enter);
        press(&mut app, 'd');
        press(&mut app, 'd');

        assert_eq!(app.store.as_ref().unwrap().row_count(), 3);
        command(&mut app, "w");
        // Asserted positively: an unchanged file is also what a *refused*
        // write leaves behind.
        assert_eq!(app.message.as_deref(), Some("wrote openthendelete.csv"));
        assert_eq!(app.store.as_ref().unwrap().dirty(), 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SAMPLE, "as it was");
    }

    #[test]
    fn a_bare_select_puts_the_columns_back() {
        let mut app = app_sized("bareselect.csv", FOURCOL, 60);
        command(&mut app, "select c");
        assert_eq!(shown_columns(&app), ["c"]);

        command(&mut app, "select");
        assert_eq!(shown_columns(&app), ["a", "b", "c", "d"]);
        assert!(app.message.is_none(), "no complaint: {:?}", app.message);

        command(&mut app, "hide a");
        command(&mut app, "select *");
        assert_eq!(shown_columns(&app), ["a", "b", "c", "d"]);
    }

    #[test]
    fn hide_drops_columns_from_what_is_on_show() {
        let mut app = app_sized("hide.csv", FOURCOL, 60);
        command(&mut app, "hide b d");
        assert_eq!(shown_columns(&app), ["a", "c"]);
    }

    #[test]
    fn a_bad_view_command_is_reported_and_changes_nothing() {
        let mut app = app_sized("badview.csv", FOURCOL, 60);
        command(&mut app, "select nope");
        assert!(app.message.clone().unwrap().contains("no column called"));
        assert_eq!(shown_columns(&app), ["a", "b", "c", "d"], "untouched");

        command(&mut app, "hide a b c d");
        assert!(app.message.clone().unwrap().contains("hide every column"));
        assert_eq!(shown_columns(&app), ["a", "b", "c", "d"]);
    }

    /// The overlay is keyed by source column, so an edit made through a
    /// reordered view has to land on the right field of the file.
    #[test]
    fn editing_through_a_narrowed_view_writes_the_right_column() {
        let (mut app, path) = app_with("viewedit.csv", FOURCOL);
        app.last_frame_width = 60;
        command(&mut app, "select d a");
        cell_mode(&mut app);

        // Display column 0 is the file's column `d`.
        press(&mut app, 'c');
        typed(&mut app, "X");
        key(&mut app, KeyCode::Enter);
        assert_eq!(shown_columns(&app), ["d", "a"]);
        assert_eq!(shown(&app, 0, 0).as_deref(), Some("X"));

        command(&mut app, "w");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "a,b,c,d\n1,2,3,X\n5,6,7,8\n",
            "the edit belongs to column d, not to column a"
        );
    }

    #[test]
    fn an_edit_hidden_by_a_view_is_still_pending_and_still_written() {
        let (mut app, path) = app_with("viewhidden.csv", FOURCOL);
        app.last_frame_width = 60;
        cell_mode(&mut app);
        press(&mut app, 'c');
        typed(&mut app, "Z");
        key(&mut app, KeyCode::Enter);

        command(&mut app, "select c d");
        assert!(
            app.store.as_ref().unwrap().edited_cells().is_empty(),
            "nowhere on screen to mark it"
        );
        assert_eq!(app.store.as_ref().unwrap().dirty(), 1, "but still pending");

        command(&mut app, "w");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "a,b,c,d\nZ,2,3,4\n5,6,7,8\n"
        );
    }

    #[test]
    fn a_narrowing_view_brings_the_cursor_back_inside_it() {
        let mut app = app_sized("clampview.csv", FOURCOL, 60);
        cell_mode(&mut app);
        press(&mut app, '$');
        assert_eq!(app.cursor_col, 3);

        command(&mut app, "select a b");
        assert_eq!(app.cursor_col, 1, "the cursor cannot point past the view");
    }

    #[test]
    fn sort_from_the_command_line_agrees_with_the_s_key() {
        let mut app = app_sized("viewsort.csv", FOURCOL, 60);
        command(&mut app, "sort b-");
        settle_sort(&mut app);
        let store = app.store.as_ref().unwrap();
        assert_eq!(store.view.sort, [(1, false)]);
        assert_eq!(store.sort_display(), [(1, false)]);
        assert_eq!(
            app.cursor_row, 0,
            "a reorder puts the cursor back at the top"
        );

        // `:sort` goes through the same rebuild the `s` key does, so the rows
        // stay identifiable and the view stays editable.
        assert_eq!(store.edit_blocked(), None);
    }

    #[test]
    fn a_sort_key_on_a_hidden_column_keeps_working_but_is_not_drawn() {
        let mut app = app_sized("hiddensort.csv", FOURCOL, 60);
        command(&mut app, "sort d");
        command(&mut app, "select a b");
        let store = app.store.as_ref().unwrap();
        assert_eq!(store.view.sort, [(3, true)], "the sort still applies");
        assert!(store.sort_display().is_empty(), "but has no header to mark");
    }

    #[test]
    fn a_selection_grows_with_the_motions() {
        let (mut app, _) = app_with("visual.csv", SAMPLE);
        cell_mode(&mut app);

        press(&mut app, 'v');
        assert_eq!(app.visual_range(), Some(((0, 0), (0, 0))));

        press(&mut app, 'j');
        press(&mut app, 'l');
        assert_eq!(app.visual_range(), Some(((0, 1), (0, 1))));

        // Counts work, because they are the ordinary motions.
        press(&mut app, 'v');
        assert_eq!(app.visual_range(), None, "v again cancels");
    }

    #[test]
    fn filling_a_selection_is_one_edit_and_one_undo() {
        let (mut app, _) = app_with("fill.csv", SAMPLE);
        cell_mode(&mut app);

        press(&mut app, 'v');
        press(&mut app, 'j');
        press(&mut app, 'l');
        press(&mut app, 'c');
        assert!(matches!(app.mode, AppMode::Edit));
        typed(&mut app, "z");
        key(&mut app, KeyCode::Enter);

        assert_eq!(app.store.as_ref().unwrap().dirty(), 4);
        assert!(app.visual_anchor.is_none(), "the selection is spent");
        for row in 0..2 {
            for col in 0..2 {
                assert_eq!(shown(&app, col, row).as_deref(), Some("z"), "{row},{col}");
            }
        }
        // Untouched by the block.
        assert_eq!(shown(&app, 0, 2).as_deref(), Some("c"));

        press(&mut app, 'u');
        assert_eq!(
            app.store.as_ref().unwrap().dirty(),
            0,
            "one fill, one undo — not four"
        );
    }

    #[test]
    fn i_and_a_build_on_what_each_cell_already_holds() {
        let (mut app, _) = app_with("prepend.csv", SAMPLE);
        cell_mode(&mut app);

        // Prepend down the name column.
        press(&mut app, 'v');
        press(&mut app, 'j');
        press(&mut app, 'i');
        typed(&mut app, ">");
        key(&mut app, KeyCode::Enter);
        assert_eq!(shown(&app, 0, 0).as_deref(), Some(">a"));
        assert_eq!(shown(&app, 0, 1).as_deref(), Some(">b"));
        assert_eq!(shown(&app, 0, 2).as_deref(), Some("c"), "outside the block");

        // Append over the same two cells — back to the top first, since the
        // cursor is left where the last selection ended.
        press(&mut app, 'g');
        press(&mut app, 'g');
        press(&mut app, 'v');
        press(&mut app, 'j');
        press(&mut app, 'a');
        typed(&mut app, "!");
        key(&mut app, KeyCode::Enter);
        assert_eq!(shown(&app, 0, 0).as_deref(), Some(">a!"));
        assert_eq!(shown(&app, 0, 1).as_deref(), Some(">b!"));
    }

    #[test]
    fn an_append_reads_rows_below_the_viewport() {
        // The page holds one row; the block covers all three. A prepend that
        // could only see the visible row would lose the other two values.
        let (mut app, path) = app_with("offpage.csv", SAMPLE);
        app.store = Some(Store::open_file(&path, 1).unwrap());
        key(&mut app, KeyCode::Tab); // column mode: every row

        press(&mut app, 'v');
        press(&mut app, 'a');
        typed(&mut app, "_x");
        key(&mut app, KeyCode::Enter);

        assert_eq!(app.store.as_ref().unwrap().dirty(), 3);
        command(&mut app, "w");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "name,count\na_x,1\nb_x,2\nc_x,3\n"
        );
    }

    #[test]
    fn a_fill_over_numbers_warns_from_the_value_it_will_write() {
        let (mut app, _) = app_with("fillwarn.csv", SAMPLE);
        key(&mut app, KeyCode::Tab); // column mode
        press(&mut app, 'l'); // the numeric column
        press(&mut app, 'v');
        press(&mut app, 'a');
        typed(&mut app, "kg");
        key(&mut app, KeyCode::Enter);

        // "1kg" is what lands in the cell, and that is what is warned about.
        let message = app.message.clone().expect("appending to a number warns");
        assert!(message.contains("becomes text"), "{message}");
        assert_eq!(shown(&app, 1, 0).as_deref(), Some("1kg"));
    }

    #[test]
    fn a_row_selection_covers_every_column() {
        let (mut app, _) = app_with("rowsel.csv", SAMPLE);
        assert_eq!(app.selection_mode, SelectionMode::Row);

        press(&mut app, 'v');
        press(&mut app, 'j');
        assert_eq!(app.visual_range(), Some(((0, 1), (0, 1))));

        press(&mut app, 'x');
        assert_eq!(app.store.as_ref().unwrap().dirty(), 4);
        // Row mode keeps its shape rather than being switched to cell mode.
        assert_eq!(app.selection_mode, SelectionMode::Row);
    }

    #[test]
    fn a_column_selection_covers_every_row() {
        let (mut app, _) = app_with("colsel.csv", SAMPLE);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.selection_mode, SelectionMode::Column);

        press(&mut app, 'v');
        assert_eq!(app.visual_range(), Some(((0, 2), (0, 0))), "all three rows");

        press(&mut app, 'x');
        assert_eq!(app.store.as_ref().unwrap().dirty(), 3);
        for row in 0..3 {
            assert_eq!(shown(&app, 0, row).as_deref(), Some(""), "row {row}");
        }
    }

    #[test]
    fn escape_gives_up_the_selection_before_the_search() {
        let (mut app, _) = app_with("esc.csv", SAMPLE);
        let query = SearchQuery::new("a".to_string()).unwrap();
        app.search_state = Some(SearchState::new(query, None));

        press(&mut app, 'v');
        key(&mut app, KeyCode::Esc);
        assert!(app.visual_anchor.is_none());
        assert!(app.search_state.is_some(), "one Esc, one thing given up");

        key(&mut app, KeyCode::Esc);
        assert!(app.search_state.is_none());
    }

    #[test]
    fn tab_gives_up_the_selection_rather_than_reshaping_it() {
        let (mut app, _) = app_with("tabsel.csv", SAMPLE);
        press(&mut app, 'v');
        key(&mut app, KeyCode::Tab);
        assert!(app.visual_anchor.is_none());
    }

    #[test]
    fn a_yanked_block_pastes_at_the_cursor() {
        let (mut app, path) = app_with("yank.csv", SAMPLE);
        cell_mode(&mut app);

        // Yank the first two names.
        press(&mut app, 'v');
        press(&mut app, 'j');
        press(&mut app, 'y');
        assert_eq!(app.message.as_deref(), Some("yanked 2 cells"));
        assert!(app.visual_anchor.is_none());
        assert_eq!(app.store.as_ref().unwrap().dirty(), 0, "yanking reads only");

        // Paste them one row down.
        press(&mut app, 'j');
        press(&mut app, 'p');
        assert_eq!(shown(&app, 0, 1).as_deref(), Some("a"));
        assert_eq!(shown(&app, 0, 2).as_deref(), Some("b"));

        command(&mut app, "w");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "name,count\na,1\na,2\nb,3\n"
        );
    }

    #[test]
    fn a_visual_yank_leaves_the_cursor_where_the_selection_began() {
        let (mut app, _) = app_with("yankcursor.csv", SAMPLE);
        cell_mode(&mut app);
        press(&mut app, 'j'); // start from row 1

        press(&mut app, 'v');
        press(&mut app, 'j'); // extend down to row 2
        press(&mut app, 'y');
        assert_eq!(app.cursor_row, 1, "back to the anchor, not the far end");
    }

    #[test]
    fn yanking_without_a_selection_follows_the_mode() {
        let (mut app, _) = app_with("yankmode.csv", SAMPLE);

        // Row mode: the whole row, every column.
        press(&mut app, 'y');
        assert_eq!(app.message.as_deref(), Some("yanked 2 cells"));

        cell_mode(&mut app);
        press(&mut app, 'y');
        assert_eq!(app.message.as_deref(), Some("yanked 1 cells"));
    }

    #[test]
    fn a_paste_running_off_the_edge_is_clipped_and_says_so() {
        let (mut app, _) = app_with("clip.csv", SAMPLE);
        cell_mode(&mut app);

        press(&mut app, 'v');
        press(&mut app, 'j');
        press(&mut app, 'y');

        // Land it on the last row, so the second cell has nowhere to go.
        press(&mut app, 'G');
        press(&mut app, 'p');
        let message = app.message.clone().unwrap();
        assert_eq!(message, "pasted 1 cells, 1 past the edge");
        assert_eq!(shown(&app, 0, 2).as_deref(), Some("a"));
        assert_eq!(app.store.as_ref().unwrap().dirty(), 1);
    }

    #[test]
    fn pasting_an_empty_register_says_so() {
        let (mut app, _) = app_with("noreg.csv", SAMPLE);
        press(&mut app, 'p');
        assert_eq!(app.message.as_deref(), Some("nothing to paste"));
        assert_eq!(app.store.as_ref().unwrap().dirty(), 0);
    }

    #[test]
    fn the_register_holds_pending_edits_not_what_is_on_disk() {
        let (mut app, _) = app_with("regedit.csv", SAMPLE);
        cell_mode(&mut app);

        press(&mut app, 'c');
        typed(&mut app, "edited");
        key(&mut app, KeyCode::Enter);
        press(&mut app, 'y');

        press(&mut app, 'j');
        press(&mut app, 'p');
        assert_eq!(shown(&app, 0, 1).as_deref(), Some("edited"));
    }

    #[test]
    fn a_fill_larger_than_the_buffer_should_hold_is_refused() {
        // A column selection covers every row, so on a large file it is the
        // easy way to ask for millions of overlay entries by accident.
        let rows = MAX_BLOCK / 2 + 1000;
        let mut csv = String::from("name,count\n");
        for i in 0..rows {
            csv.push_str(&format!("r{i},{i}\n"));
        }
        let (mut app, _) = app_with("huge.csv", &csv);
        key(&mut app, KeyCode::Tab); // column mode
        press(&mut app, 'v');
        press(&mut app, 'l'); // both columns, every row

        press(&mut app, 'c');
        assert!(matches!(app.mode, AppMode::Normal), "no editor opens");
        assert_eq!(app.store.as_ref().unwrap().dirty(), 0);
        let message = app.message.clone().unwrap();
        assert!(message.contains("too many"), "{message}");
    }

    /// The picker holds a working copy, so leaving it costs nothing.
    #[test]
    fn escape_leaves_the_picker_changing_nothing() {
        let mut app = app_sized("pick.csv", FOURCOL, 60);
        press(&mut app, 'C');
        assert!(matches!(app.mode, AppMode::Picker));

        press(&mut app, 'j');
        press(&mut app, '-'); // untick b
        press(&mut app, 'p'); // and pin it
        key(&mut app, KeyCode::Esc);

        assert!(matches!(app.mode, AppMode::Normal));
        assert!(app.picker.is_none());
        assert_eq!(shown_columns(&app), ["a", "b", "c", "d"], "untouched");
        assert!(app.pinned.is_empty(), "and the pin never happened");
    }

    /// Enter writes a `:select`, so the picker is a way of *typing* one rather
    /// than a second thing that decides what shows.
    #[test]
    fn the_picker_applies_its_selection_as_a_select() {
        let mut app = app_sized("pickapply.csv", FOURCOL, 60);
        press(&mut app, 'C');
        press(&mut app, 'j');
        press(&mut app, '-'); // untick b
        key(&mut app, KeyCode::Enter);

        assert!(matches!(app.mode, AppMode::Normal));
        assert_eq!(shown_columns(&app), ["a", "c", "d"]);
        // It went through the view, so the view describes it and reset undoes it.
        command(&mut app, "reset select");
        assert_eq!(shown_columns(&app), ["a", "b", "c", "d"]);
    }

    /// A reordering `:select` has to survive a round trip: a picker that
    /// quietly discards the order would be worse than no picker.
    #[test]
    fn opening_and_applying_the_picker_preserves_the_view_order() {
        let mut app = app_sized("pickorder.csv", FOURCOL, 60);
        command(&mut app, "select c a");
        assert_eq!(shown_columns(&app), ["c", "a"]);

        press(&mut app, 'C');
        key(&mut app, KeyCode::Enter);
        assert_eq!(shown_columns(&app), ["c", "a"], "unchanged");
    }

    /// A column ticked back on lands where it is listed, which is where the
    /// user was looking when they ticked it.
    #[test]
    fn a_column_ticked_back_on_lands_where_it_was_listed() {
        let mut app = app_sized("pickback.csv", FOURCOL, 60);
        command(&mut app, "select a c");
        press(&mut app, 'C');
        // Listed a c b d — the view's two, then the hidden ones.
        press(&mut app, 'j');
        press(&mut app, 'j'); // onto b
        press(&mut app, '-');
        key(&mut app, KeyCode::Enter);
        assert_eq!(shown_columns(&app), ["a", "c", "b"]);
    }

    #[test]
    fn the_picker_carries_pins_in_and_back_out() {
        let mut app = app_sized("pickpin.csv", FOURCOL, 60);
        key(&mut app, KeyCode::Tab);
        press(&mut app, 'z');
        press(&mut app, 'p'); // pin a the ordinary way

        press(&mut app, 'C');
        press(&mut app, 'j');
        press(&mut app, 'p'); // and b from the list
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.pinned.iter().copied().collect::<Vec<_>>(), [0, 1]);

        // Unpinning from the list works the same way round.
        press(&mut app, 'C');
        press(&mut app, 'p');
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.pinned.iter().copied().collect::<Vec<_>>(), [1]);
    }

    /// The view change is what the user came for, so pins that do not fit are
    /// trimmed and named rather than taking the whole apply down with them.
    #[test]
    fn pins_that_do_not_fit_are_trimmed_not_refused() {
        let mut app = app_sized("picktrim.csv", FOURCOL, 24);
        press(&mut app, 'C');
        press(&mut app, 'p'); // pin a
        press(&mut app, 'j');
        press(&mut app, 'p'); // and b
        press(&mut app, 'j');
        press(&mut app, '-'); // while also hiding c, so the view change is real
        key(&mut app, KeyCode::Enter);

        assert_eq!(shown_columns(&app), ["a", "b", "d"], "the view still applied");
        assert!(
            app.pinned.len() < 2,
            "and the pins were trimmed to what fits: {:?}",
            app.pinned
        );
        let said = app.message.clone().unwrap();
        assert!(said.contains("no room for"), "{said}");
    }

    /// Unticking the last column is stopped where it is pressed, rather than
    /// on Enter after a whole session of ticking.
    #[test]
    fn the_picker_refuses_to_untick_the_last_column() {
        let mut app = app_sized("picklast.csv", FOURCOL, 60);
        command(&mut app, "select a");
        press(&mut app, 'C');
        press(&mut app, '-');
        let refusal = app.message.clone().unwrap();
        assert!(refusal.contains("hide every column"), "{refusal}");

        key(&mut app, KeyCode::Enter);
        assert_eq!(shown_columns(&app), ["a"]);
    }

    /// Unlike `-`, the picker is a list of every column rather than an
    /// operation on the one under the cursor, so it needs no column cursor.
    #[test]
    fn the_picker_opens_in_row_mode() {
        let mut app = app_sized("pickrow.csv", FOURCOL, 60);
        assert_eq!(app.selection_mode, SelectionMode::Row);
        press(&mut app, 'C');
        assert!(matches!(app.mode, AppMode::Picker));
        press(&mut app, 'j');
        press(&mut app, '-');
        key(&mut app, KeyCode::Enter);
        assert_eq!(shown_columns(&app), ["a", "c", "d"]);
    }

    /// The workflow the keys exist for: start from none, tick the few wanted.
    #[test]
    fn a_and_shift_a_pick_a_handful_out_of_the_whole_list() {
        let mut app = app_sized("pickall.csv", FOURCOL, 60);
        press(&mut app, 'C');
        press(&mut app, 'j'); // onto b
        press(&mut app, 'A'); // and nothing else
        press(&mut app, 'j');
        press(&mut app, 'j'); // onto d
        press(&mut app, '-'); // tick it too
        key(&mut app, KeyCode::Enter);
        assert_eq!(shown_columns(&app), ["b", "d"]);

        press(&mut app, 'C');
        press(&mut app, 'a');
        key(&mut app, KeyCode::Enter);
        assert_eq!(shown_columns(&app), ["b", "d", "a", "c"], "all back, listed order");
    }

    /// `q` closes a window everywhere else in vim, and a window is a view —
    /// closing one never destroys work. The picker holds changes that are not
    /// applied yet, so it cannot close harmlessly, and it does not borrow the
    /// letter for something that would throw them away.
    #[test]
    fn q_is_not_a_picker_key() {
        let mut app = app_sized("pickq.csv", FOURCOL, 60);
        press(&mut app, 'C');
        press(&mut app, 'j');
        press(&mut app, '-'); // untick b

        press(&mut app, 'q');
        assert!(matches!(app.mode, AppMode::Picker), "still open");
        assert!(!app.exit, "and it is not the app's q either");

        key(&mut app, KeyCode::Enter);
        assert_eq!(shown_columns(&app), ["a", "c", "d"], "the tick survived it");
    }

    /// The picker's keys are the least guessable in plv, and the status bar
    /// drops its hints on a narrow terminal, so `?` has to reach them.
    #[test]
    fn the_help_overlay_follows_the_picker() {
        let mut app = app_sized("pickhelp.csv", FOURCOL, 60);
        press(&mut app, 'C');
        press(&mut app, '?');
        assert!(app.help_visible);
        let sections = app.help_sections();
        assert_eq!(sections.len(), 1, "just the picker's own keys");
        let keys: Vec<&str> = sections[0].1.iter().map(|(key, _)| *key).collect();
        assert!(keys.contains(&"-") && keys.contains(&"p"), "{keys:?}");
    }

    /// `-` is `:hide <name>` without the typing, so it goes through the same
    /// slot and shows up in the same place.
    #[test]
    fn minus_hides_the_cursor_column() {
        let mut app = app_sized("hide.csv", FOURCOL, 60);
        key(&mut app, KeyCode::Tab); // column mode
        press(&mut app, 'l'); // onto b

        press(&mut app, '-');
        assert_eq!(shown_columns(&app), ["a", "c", "d"]);
    }

    /// `:hide` reads the current list and filters it, so hiding twice narrows
    /// twice — where a second `:select` would replace the first.
    #[test]
    fn hiding_accumulates_rather_than_replacing() {
        let mut app = app_sized("hidetwice.csv", FOURCOL, 60);
        key(&mut app, KeyCode::Tab);
        press(&mut app, '-');
        press(&mut app, '-');
        assert_eq!(shown_columns(&app), ["c", "d"]);
    }

    /// The cursor stays where the column was, on whatever moved into that
    /// position — as `dd` leaves it on the next row.
    #[test]
    fn the_cursor_stays_where_the_hidden_column_was() {
        let mut app = app_sized("hidecursor.csv", FOURCOL, 60);
        key(&mut app, KeyCode::Tab);
        press(&mut app, 'l'); // onto b, position 1
        press(&mut app, '-');
        assert_eq!(app.cursor_col, 1, "still position 1");
        assert_eq!(shown_columns(&app)[app.cursor_col], "c", "now showing c");

        // At the right edge there is nothing to move up, so it steps back.
        press(&mut app, '$');
        let last = shown_columns(&app).len() - 1;
        assert_eq!(app.cursor_col, last);
        press(&mut app, '-');
        assert_eq!(app.cursor_col, shown_columns(&app).len() - 1);
    }

    /// Hiding everything would leave nothing to look at, and `view::apply`
    /// already refuses it — the key must not find its own way round that.
    #[test]
    fn hiding_the_last_column_is_refused() {
        let mut app = app_sized("hideall.csv", FOURCOL, 60);
        key(&mut app, KeyCode::Tab);
        for _ in 0..3 {
            press(&mut app, '-');
        }
        assert_eq!(shown_columns(&app).len(), 1);

        press(&mut app, '-');
        assert_eq!(shown_columns(&app).len(), 1, "the last one stays");
        let refusal = app.message.clone().unwrap();
        assert!(refusal.contains("hide every column"), "{refusal}");
    }

    /// There is no key that un-hides one column, so the way back has to be in
    /// front of the user at the moment they might want it.
    #[test]
    fn hiding_says_how_to_get_the_column_back() {
        let mut app = app_sized("hideback.csv", FOURCOL, 60);
        key(&mut app, KeyCode::Tab);
        press(&mut app, '-');
        let said = app.message.clone().unwrap();
        assert!(said.contains("hid a"), "{said}");
        assert!(said.contains("reset select"), "{said}");

        command(&mut app, "reset select");
        assert_eq!(shown_columns(&app), ["a", "b", "c", "d"]);
    }

    /// Row mode has no column cursor, so there is no column the key could
    /// mean — the same gate `s` has.
    #[test]
    fn hiding_needs_a_column_cursor() {
        let mut app = app_sized("hiderow.csv", FOURCOL, 60);
        assert_eq!(app.selection_mode, SelectionMode::Row);
        press(&mut app, '-');
        assert_eq!(shown_columns(&app), ["a", "b", "c", "d"]);
    }

    /// The two column features compose: a pin on a hidden column waits, and
    /// hiding around a pin leaves it drawn.
    #[test]
    fn a_hidden_column_keeps_its_pin_for_when_it_comes_back() {
        let mut app = app_sized("hidepin.csv", FOURCOL, 60);
        key(&mut app, KeyCode::Tab);
        press(&mut app, 'z');
        press(&mut app, 'p'); // pin a
        press(&mut app, '-'); // and hide it

        assert_eq!(shown_columns(&app), ["b", "c", "d"]);
        assert!(app.display_pins().is_empty(), "nowhere to draw it");
        assert_eq!(app.pinned.iter().copied().collect::<Vec<_>>(), [0]);

        command(&mut app, "reset select");
        assert_eq!(app.display_pins().iter().copied().collect::<Vec<_>>(), [0]);
    }

    #[test]
    fn zp_pins_the_cursor_column_and_pressing_it_again_lets_go() {
        let mut app = app_sized("pin.csv", FOURCOL, 60);
        key(&mut app, KeyCode::Tab); // column mode, so there is a column cursor
        press(&mut app, 'l'); // onto b

        press(&mut app, 'z');
        press(&mut app, 'p');
        assert_eq!(app.pinned.iter().copied().collect::<Vec<_>>(), [1]);

        press(&mut app, 'z');
        press(&mut app, 'p');
        assert!(app.pinned.is_empty(), "the same key lets it go");
    }

    #[test]
    fn z_bar_unpins_everything_at_once() {
        let mut app = app_sized("unpinall.csv", FOURCOL, 60);
        key(&mut app, KeyCode::Tab);
        press(&mut app, 'z');
        press(&mut app, 'p');
        press(&mut app, 'l');
        press(&mut app, 'z');
        press(&mut app, 'p');
        assert_eq!(app.pinned.len(), 2);

        press(&mut app, 'z');
        press(&mut app, '|');
        assert!(app.pinned.is_empty());

        // `zP` is `zp`: the prefix lowercases its second key, so a stray shift
        // toggles the cursor column rather than clearing the lot.
        press(&mut app, 'z');
        press(&mut app, 'P');
        assert_eq!(app.pinned.len(), 1);
    }

    /// A pin is kept against the source column, like a width, so reordering
    /// the view moves the pin with its column rather than leaving it on
    /// whatever now happens to sit in that position.
    #[test]
    fn a_pin_follows_its_column_through_a_select() {
        let mut app = app_sized("pinselect.csv", FOURCOL, 60);
        key(&mut app, KeyCode::Tab);
        press(&mut app, 'z');
        press(&mut app, 'p'); // pin `a`, source 0, display 0
        assert_eq!(app.display_pins().iter().copied().collect::<Vec<_>>(), [0]);

        command(&mut app, "select c a");
        assert_eq!(shown_columns(&app), ["c", "a"]);
        assert_eq!(
            app.pinned.iter().copied().collect::<Vec<_>>(),
            [0],
            "still `a`"
        );
        assert_eq!(
            app.display_pins().iter().copied().collect::<Vec<_>>(),
            [1],
            "which `select` has moved to the second position"
        );
    }

    /// A pin on a column the view hides is not forgotten — it has nowhere to
    /// be drawn, and comes back when the column does.
    #[test]
    fn a_pin_on_a_hidden_column_waits_rather_than_being_dropped() {
        let mut app = app_sized("pinhide.csv", FOURCOL, 60);
        key(&mut app, KeyCode::Tab);
        press(&mut app, 'z');
        press(&mut app, 'p');

        command(&mut app, "select b c");
        assert!(app.display_pins().is_empty(), "nowhere to draw it");
        assert_eq!(app.pinned.iter().copied().collect::<Vec<_>>(), [0]);

        command(&mut app, "reset select");
        assert_eq!(app.display_pins().iter().copied().collect::<Vec<_>>(), [0]);
    }

    /// Pinning the whole width would leave a table that cannot be moved
    /// through and nothing on screen to say why, so the pin is refused and
    /// says so — the call `sort_blocked` makes.
    #[test]
    fn a_pin_that_would_leave_nothing_to_scroll_in_is_refused() {
        let mut app = app_sized("pinfull.csv", FOURCOL, 24);
        key(&mut app, KeyCode::Tab);

        let mut pinned = 0;
        for _ in 0..4 {
            press(&mut app, 'z');
            press(&mut app, 'p');
            if app.message.is_some() {
                break;
            }
            pinned += 1;
            press(&mut app, 'l');
        }
        assert!(pinned > 0, "some of them fit");
        assert!(pinned < 4, "not all of them");
        assert_eq!(app.pinned.len(), pinned, "the refused one is not kept");
        let refusal = app.message.clone().unwrap();
        assert!(refusal.contains("no room to pin"), "{refusal}");
    }

    /// A pinned column is on screen at every offset, so the cursor landing on
    /// one must not drag the view back to where that column lives.
    #[test]
    fn the_cursor_on_a_pinned_column_does_not_scroll_the_view() {
        let mut app = app_sized("pinscroll.csv", WIDE, 40);
        key(&mut app, KeyCode::Tab);
        press(&mut app, 'z');
        press(&mut app, 'p'); // pin the first column

        for _ in 0..5 {
            press(&mut app, 'l');
        }
        app.col_offset = 4;
        let before = app.col_offset;

        app.cursor_col = 0; // back onto the pin, which is drawn regardless
        app.column_left(0);
        assert_eq!(app.col_offset, before, "the view stays where it was");
    }

    #[test]
    fn an_unknown_command_says_so() {
        let (mut app, _) = app_with("unknown.csv", SAMPLE);
        command(&mut app, "nope");
        assert_eq!(app.message.as_deref(), Some("not a command: :nope"));
        assert!(!app.exit);
    }

    #[test]
    fn k_shows_the_cursor_cell_and_follows_it() {
        let long = "a,note\n1,\"a value far too long for any column to show\"\n2,short\n";
        let mut app = app_sized("cellview.csv", long, 40);
        assert_eq!(app.panel_height(40, 24), 0, "nothing showing yet");

        press(&mut app, 'K');
        assert!(app.cell_view);
        assert_eq!(
            app.selection_mode,
            SelectionMode::Cell,
            "a cell view needs a cell cursor, as an edit does"
        );
        let (name, value) = app.cell_under_cursor().unwrap();
        assert_eq!(name, "a");
        assert_eq!(value, "1");

        // It follows the cursor rather than freezing on one cell.
        press(&mut app, 'l');
        let (name, value) = app.cell_under_cursor().unwrap();
        assert_eq!(name, "note");
        assert!(value.starts_with("a value far too long"));
        assert!(app.panel_height(40, 24) > 1, "and takes room to show it");

        key(&mut app, KeyCode::Esc);
        assert!(!app.cell_view);
        assert_eq!(app.panel_height(40, 24), 0);
    }

    #[test]
    fn the_cell_view_gives_up_rows_to_show_itself() {
        let long = "a\n\"".to_string() + &"x".repeat(500) + "\"\n";
        let mut app = app_sized("cellroom.csv", &long, 40);
        let before = App::viewport_rows(24, 0);
        press(&mut app, 'K');
        let panel = app.panel_height(40, 24);
        assert!(panel > 0);
        assert_eq!(
            App::viewport_rows(24, panel),
            before - panel as usize,
            "the table gives up exactly what the panel takes"
        );
        assert!(panel <= 12, "and never more than half the screen");
    }

    #[test]
    fn a_column_can_be_widened_narrowed_and_put_back() {
        let mut app = app_sized("widths.csv", FOURCOL, 60);
        cell_mode(&mut app);
        let start = app.drawn_width(0);

        press(&mut app, 'z');
        press(&mut app, '>');
        let wider = *app.widths.get(&0).expect("a width was set");
        assert!(wider > start, "{wider} should be wider than {start}");

        press(&mut app, 'z');
        press(&mut app, '<');
        assert_eq!(*app.widths.get(&0).unwrap(), start, "back where it began");

        press(&mut app, 'z');
        press(&mut app, '=');
        assert!(app.widths.is_empty(), "and z= clears the lot");
    }

    /// `<`, `>` and `_` all need shift, and the shift goes down before the
    /// `z` does — so both keys arrive capitalised and neither half matched.
    #[test]
    fn shift_held_through_a_z_command_still_works() {
        let mut app = app_sized("shiftz.csv", FOURCOL, 60);
        cell_mode(&mut app);
        let start = app.drawn_width(0);

        // Shift held from before the prefix: `Z` then `>`.
        key(
            &mut app,
            KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::SHIFT),
        );
        assert_eq!(app.pending_prefix, Some('z'), "`Z` opens the prefix too");
        key(
            &mut app,
            KeyEvent::new(KeyCode::Char('>'), KeyModifiers::SHIFT),
        );
        assert!(app.widths.get(&0).is_some_and(|&w| w > start));
    }

    #[test]
    fn a_shifted_second_key_still_reaches_the_unshifted_command() {
        // A file with room to scroll: FOURCOL has two rows, so `zt` would
        // have nowhere to put anything.
        let mut app = tall_app("shiftzz.csv");
        app.cursor_to(20).unwrap();
        // `ZT` should be `zt`, not nothing.
        key(
            &mut app,
            KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::SHIFT),
        );
        key(
            &mut app,
            KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT),
        );
        assert_eq!(
            app.store.as_ref().unwrap().row_offset,
            20,
            "zt put the cursor row at the top"
        );
    }

    #[test]
    fn a_width_belongs_to_the_column_not_to_its_place_on_screen() {
        // `:select` renumbers the display positions; a width set before it
        // has to follow its own column.
        let mut app = app_sized("widthview.csv", FOURCOL, 60);
        cell_mode(&mut app);
        press(&mut app, 'l'); // display 1 is source column `b`
        press(&mut app, 'z');
        press(&mut app, '>');
        let set = *app.widths.get(&1).expect("keyed by source column");

        command(&mut app, "select d b");
        assert_eq!(
            app.widths.get(&1),
            Some(&set),
            "`b` keeps its width at its new position"
        );
    }

    #[test]
    fn fitting_a_column_uses_the_widest_value_on_screen() {
        let mut app = app_sized("fit.csv", "a,b\nshort,x\nmuch longer value,y\n", 60);
        cell_mode(&mut app);
        press(&mut app, 'z');
        press(&mut app, '_');
        assert_eq!(
            app.widths.get(&0),
            Some(&"much longer value".len()),
            "the widest value on the page, not a guess"
        );
    }

    #[test]
    fn a_column_cannot_be_narrowed_away_entirely() {
        let mut app = app_sized("narrow.csv", FOURCOL, 60);
        cell_mode(&mut app);
        for _ in 0..20 {
            press(&mut app, 'z');
            press(&mut app, '<');
        }
        assert_eq!(*app.widths.get(&0).unwrap(), ui::MIN_COLUMN);
    }

    #[test]
    fn hash_switches_between_relative_and_absolute_row_numbers() {
        let (mut app, _) = app_with("gutter.csv", SAMPLE);
        assert!(app.relative_rows, "counting from the cursor is the default");

        press(&mut app, '#');
        assert!(!app.relative_rows);
        press(&mut app, '#');
        assert!(app.relative_rows);
    }

    #[test]
    fn a_question_mark_is_a_character_while_typing() {
        let (mut app, _) = app_with("help.csv", SAMPLE);

        press(&mut app, 'c');
        press(&mut app, '?');
        assert!(!app.help_visible, "'?' belongs to the cell being typed");
        assert_eq!(app.edit_buf, "?");

        key(&mut app, KeyCode::Esc);
        press(&mut app, '?');
        assert!(app.help_visible, "and opens the overlay in normal mode");
    }

    /// Sorting runs off the main thread; drain it the way the event loop does.
    fn settle_sort(app: &mut App) {
        for _ in 0..2000 {
            if app.sort_rx.is_none() {
                return;
            }
            if !app.poll_sort() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        panic!("the sort never finished");
    }

    #[test]
    fn a_sorted_view_can_be_edited_once_the_sort_is_held() {
        let (mut app, path) = app_with("sorted.csv", SAMPLE);
        app.last_frame_width = 60;
        cell_mode(&mut app);
        press(&mut app, 's'); // by name, ascending
        settle_sort(&mut app);
        press(&mut app, 's'); // and again: descending, so `d` is on top
        settle_sort(&mut app);

        press(&mut app, 'l');
        press(&mut app, 'c');
        typed(&mut app, "99");
        key(&mut app, KeyCode::Enter);
        assert_eq!(shown(&app, 1, 0).as_deref(), Some("99"));

        command(&mut app, "w");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "name,count\na,1\nb,2\nc,99\n",
            "the top row of a descending sort is the file's last line"
        );
    }

    #[test]
    fn editing_the_caret_inside_the_buffer() {
        let (mut app, _) = app_with("caretmove.csv", SAMPLE);

        press(&mut app, 'c');
        typed(&mut app, "abc");
        key(&mut app, KeyCode::Left);
        typed(&mut app, "X");
        assert_eq!(app.edit_buf, "abXc");

        key(&mut app, KeyCode::Home);
        key(&mut app, KeyCode::Delete);
        assert_eq!(app.edit_buf, "bXc");

        key(&mut app, KeyCode::End);
        key(&mut app, KeyCode::Backspace);
        assert_eq!(app.edit_buf, "bX");
    }
}
