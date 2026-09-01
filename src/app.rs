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

use crate::data::Store;
use crate::data::lake_db::{self, LakeDb};
use crate::lake::{Lake, Level, Scope};
use crate::search::{SearchQuery, SearchState, SearchStatus};
use crate::ui::{Browser, DataTable, Help, Prompt, Section, SelectionMode, StatusBar, Theme};
use polars::prelude::DataType;

enum AppMode {
    Normal,
    Search,
    /// Typing a new value for one cell. What is typed goes into the edit
    /// buffer, never straight to disk — `:w` writes.
    Edit,
    /// Typing an ex command after `:`.
    Command,
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
    pending_z: bool,
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
    search_buf: String,
    search_state: Option<SearchState>,
    /// Receives batches of matching row indices from the background search thread.
    /// Dropping this cancels the search.
    search_rx: Option<mpsc::Receiver<Vec<usize>>>,
    /// Receives the sorted first-page DataFrame from the background sort thread.
    sort_rx: Option<mpsc::Receiver<polars::prelude::DataFrame>>,
    /// Incremented each draw while a background task is running; drives animations.
    spinner_tick: usize,
    /// Present when the opened path was a DuckLake catalog rather than a file.
    lake: Option<Lake>,
    screen: Screen,
    /// Viewport height captured each frame, so key handlers can size a new Store.
    last_vp: usize,
    /// The `?` key-binding overlay is showing.
    help_visible: bool,
}

impl App {
    pub fn new(file: Option<PathBuf>) -> Self {
        Self {
            file_path: file,
            store: None,
            col_offset: 0,
            cursor_row: 0,
            pending_num: String::new(),
            pending_z: false,
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
            search_buf: String::new(),
            search_state: None,
            search_rx: None,
            sort_rx: None,
            spinner_tick: 0,
            lake: None,
            screen: Screen::Viewer,
            last_vp: 20,
            help_visible: false,
        }
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
        if let Some(path) = self.file_path.clone() {
            let size = terminal.size()?;
            let vp = Self::viewport_rows(size.height);
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

    // status bar (1) + block borders (2) + header row (1) = 4 overhead
    fn viewport_rows(terminal_height: u16) -> usize {
        (terminal_height as usize).saturating_sub(4)
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        self.last_frame_width = area.width;

        let vp = Self::viewport_rows(area.height);
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

        // In Column/Cell mode keep cursor_col within [col_offset, last_vis_col].
        // Must be correct in a single pass: handle_events blocks on event::read
        // when no search is active, so multi-frame convergence never fires.
        if !matches!(self.selection_mode, SelectionMode::Row) {
            if self.cursor_col < self.col_offset {
                self.col_offset = self.cursor_col;
            } else if self.cursor_col > self.last_vis_col {
                // cursor is off the right edge — compute the col_offset that
                // places cursor_col at the rightmost visible position.
                self.col_offset = self.col_offset_to_show_at_right(self.cursor_col);
            }
        }

        let [table_area, status_area] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(area);

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
            if self.search_rx.is_some() || self.sort_rx.is_some() {
                self.spinner_tick = self.spinner_tick.wrapping_add(1);
            }

            let cursor_col = match self.selection_mode {
                SelectionMode::Row => col_offset,
                _ => self.cursor_col,
            };
            let edited = store.edited_cells();

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
                    sort: &store.sort,
                    sort_tick: self.sort_rx.as_ref().map(|_| self.spinner_tick),
                    edited: &edited,
                    selection: self.visual_range(),
                },
                table_area,
            );

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
                            .and_then(|(_, col)| store.schema.get_at_index(col))
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
                AppMode::Normal => {
                    let search_info = self.search_state.as_ref().map(|s| {
                        let (cur, total, complete) = s.match_info();
                        (s.query.raw.clone(), cur, total, complete)
                    });
                    frame.render_widget(
                        StatusBar {
                            file_name,
                            cursor_row,
                            total_rows: store.total_rows,
                            col_offset,
                            total_cols: store.schema.len(),
                            message: self.message.clone(),
                            dirty: store.dirty(),
                            selection: self
                                .visual_range()
                                .map(|((r0, r1), (c0, c1))| (r1 - r0 + 1, c1 - c0 + 1)),
                            pending_num: self.pending_num.clone(),
                            pending_z: self.pending_z,
                            theme: &self.theme,
                            search_info,
                            spinner_tick: self.spinner_tick,
                            sort_tick: self.sort_rx.as_ref().map(|_| self.spinner_tick),
                            help: if self.lake.is_some() {
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
            ("Ctrl+d / Ctrl+u", "Half page down / up"),
            ("g / G", "First / last row"),
            ("{n}G", "Jump to row n"),
            ("zz / zt / zb", "Centre / top / bottom"),
        ];
        const COLUMNS: &[(&str, &str)] = &[
            ("h / l", "Scroll columns left / right"),
            ("H", "First column"),
            ("0 / $", "First / last column (column mode)"),
            ("Tab", "Cycle row \u{2192} column \u{2192} cell"),
            ("s", "Sort by cursor column"),
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
            ("u / Ctrl+r", "Undo / redo"),
            ("y / p", "Yank the cursor / paste at the cursor"),
            (
                "Tab / Shift+Tab",
                "While editing: commit, next / previous cell",
            ),
            (":w   :w!   :w path", "Write (force / elsewhere)"),
            (":q   :q!   :wq", "Quit (discarding / writing)"),
        ];
        const VISUAL: &[(&str, &str)] = &[
            ("v", "Start or cancel a selection"),
            ("", "Its shape follows the Tab mode"),
            ("c", "Replace every cell with one value"),
            ("i / a", "Prepend / append text to every cell"),
            ("x / d", "Clear the selection"),
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
            ("General", GENERAL),
        ];
        const BROWSER: &[Section<'static>] = &[("Catalog", BROWSE), ("General", GENERAL)];

        if self.screen == Screen::Browser {
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
    }

    fn handle_events(&mut self) -> io::Result<()> {
        // If poll_sort or poll_search changed any state, return immediately so
        // the main loop redraws before blocking on input.
        if self.poll_sort() || self.poll_search() {
            return Ok(());
        }

        // While any background task is running use a short timeout so the UI
        // redraws as result batches arrive. When idle, block on read directly.
        if (self.search_rx.is_some() || self.sort_rx.is_some())
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
                    if let Some(state) = &mut self.search_state {
                        state.matching_rows.extend(rows);
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
                    store.set_view(df);
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
        // Only in Normal mode: '?' is an ordinary character to type into a
        // search pattern, a cell or a command.
        if key.code == KeyCode::Char('?') && matches!(self.mode, AppMode::Normal) {
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
                                SelectionMode::Column | SelectionMode::Cell => store
                                    .schema
                                    .get_at_index(self.cursor_col)
                                    .map(|(name, _)| name.to_string()),
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

        // Resolve pending z-prefix
        if self.pending_z {
            self.pending_z = false;
            return match key.code {
                KeyCode::Char('z') => self.scroll_center(),
                KeyCode::Char('t') => self.scroll_cursor_top(),
                KeyCode::Char('b') => self.scroll_cursor_bottom(),
                _ => Ok(()),
            };
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => self.quit(false),

            // In column/cell mode '0' jumps to the first column (vim-style).
            // Must come before the digit-accumulation arm.
            KeyCode::Char('0')
                if !ctrl
                    && matches!(
                        self.selection_mode,
                        SelectionMode::Column | SelectionMode::Cell
                    ) =>
            {
                self.pending_num.clear();
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
            KeyCode::Char('d') if ctrl => {
                let vp = self.store.as_ref().map_or(1, |s| s.viewport_rows);
                self.cursor_down((vp / 2).max(1))?;
            }
            KeyCode::Char('u') if ctrl => {
                let vp = self.store.as_ref().map_or(1, |s| s.viewport_rows);
                self.cursor_up((vp / 2).max(1))?;
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.pending_num.clear();
                self.cursor_to(0)?;
            }
            KeyCode::Char('G') | KeyCode::End => {
                if self.pending_num.is_empty() {
                    let last = self
                        .store
                        .as_ref()
                        .map_or(0, |s| s.total_rows.saturating_sub(1));
                    self.cursor_to(last)?;
                } else {
                    let s = std::mem::take(&mut self.pending_num);
                    self.jump_to_line(&s)?;
                }
            }

            // z-prefix: zz (center), zt (top), zb (bottom)
            KeyCode::Char('z') => {
                self.pending_num.clear();
                self.pending_z = true;
            }

            // Column navigation
            KeyCode::Char('h') | KeyCode::Left => {
                self.pending_num.clear();
                match self.selection_mode {
                    SelectionMode::Row => {
                        self.col_offset = self.col_offset.saturating_sub(1);
                    }
                    SelectionMode::Column | SelectionMode::Cell => {
                        self.cursor_col = self.cursor_col.saturating_sub(1);
                        if self.cursor_col < self.col_offset {
                            self.col_offset = self.cursor_col;
                        }
                    }
                }
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.pending_num.clear();
                match self.selection_mode {
                    SelectionMode::Row => {
                        if let Some(store) = &self.store {
                            let max_col = store.schema.len().saturating_sub(1);
                            if self.col_offset < max_col {
                                self.col_offset += 1;
                            }
                        }
                    }
                    SelectionMode::Column | SelectionMode::Cell => {
                        if let Some(store) = &self.store {
                            let max_col = store.schema.len().saturating_sub(1);
                            if self.cursor_col < max_col {
                                self.cursor_col += 1;
                                if self.cursor_col > self.last_vis_col {
                                    self.col_offset += 1;
                                }
                            }
                        }
                    }
                }
            }
            // Jump to first column (all modes) or last column (Column/Cell via $).
            KeyCode::Char('H') => {
                self.pending_num.clear();
                self.col_offset = 0;
                self.cursor_col = 0;
            }
            KeyCode::Char('$')
                if matches!(
                    self.selection_mode,
                    SelectionMode::Column | SelectionMode::Cell
                ) =>
            {
                self.pending_num.clear();
                if let Some(store) = &self.store {
                    self.cursor_col = store.schema.len().saturating_sub(1);
                    // col_offset will be corrected in draw() before the next render.
                }
            }

            // Sort by cursor column (Column/Cell mode only). Toggles asc ↔ desc;
            // pressing s on a new column adds it as the next priority sort key.
            KeyCode::Char('s')
                if matches!(
                    self.selection_mode,
                    SelectionMode::Column | SelectionMode::Cell
                ) =>
            {
                self.visual_anchor = None;
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
            KeyCode::Char('x') | KeyCode::Char('d') if self.visual_anchor.is_some() => {
                self.clear_selection()?
            }
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

            KeyCode::Esc if self.visual_anchor.is_some() => {
                self.pending_num.clear();
                self.visual_anchor = None;
            }
            KeyCode::Esc => {
                self.search_state = None;
                self.search_rx = None; // dropping rx cancels background scan
                self.pending_num.clear();
                if let Some(store) = &mut self.store
                    && !store.sort.is_empty()
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
        let all_rows = (0, store.total_rows.saturating_sub(1));
        let all_cols = (0, store.schema.len().saturating_sub(1));
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
        let last_row = store.total_rows.saturating_sub(1);
        let last_col = store.schema.len().saturating_sub(1);

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
            .map_or(0, |s| s.schema.len().saturating_sub(1));
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
        let (name, dtype) = self.store.as_ref()?.schema.get_at_index(col)?;
        let fits = match dtype {
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
            }
            KeyCode::Enter => {
                let line = std::mem::take(&mut self.command_buf);
                self.mode = AppMode::Normal;
                self.run_command(line.trim())?;
            }
            KeyCode::Backspace => {
                if self.command_buf.pop().is_none() {
                    // Backspacing off the `:` leaves the command line, as in vim.
                    self.mode = AppMode::Normal;
                }
            }
            KeyCode::Char(c) if !ctrl => self.command_buf.push(c),
            _ => {}
        }
        Ok(())
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
            other => self.message = Some(format!("not a command: :{other}")),
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

    // ── cursor + scroll helpers ────────────────────────────────────────────

    /// Move cursor to `row` (0-based), scrolling the viewport only if needed.
    fn cursor_to(&mut self, row: usize) -> anyhow::Result<()> {
        let (total, vp, offset) = match &self.store {
            Some(s) => (s.total_rows, s.viewport_rows, s.row_offset),
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
        let total = self.store.as_ref().map_or(0, |s| s.total_rows);
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
            Some(s) => (s.total_rows, s.viewport_rows),
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
            Some(s) => (s.total_rows, s.viewport_rows),
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
        let total = self.store.as_ref().map_or(0, |s| s.total_rows);
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
        const SP: usize = 4; // COLUMN_SPACING
        const MAX_COL_FRAC: f32 = 0.3;
        const MIN_COL_WIDTH: usize = 3;

        let store = match &self.store {
            Some(s) => s,
            None => return 0,
        };
        let df = &store.current_view;
        let cols = df.columns();
        if cols.is_empty() {
            return 0;
        }
        let cursor_col = cursor_col.min(cols.len() - 1);

        let inner_w = (self.last_frame_width as usize).saturating_sub(2);
        let max_col = ((inner_w as f32 * MAX_COL_FRAC) as usize).max(MIN_COL_WIDTH);

        // row_num_w — same formula as DataTable::row_num_width
        let horizon = (store.row_offset + df.height() * 3).max(99);
        let mut p: usize = 10;
        while p <= horizon {
            p *= 10;
        }
        let row_num_w = (p.to_string().len() - 1) + 2;

        // Natural width for a column (header vs data max).
        let nat = |ci: usize| -> usize {
            let c = &cols[ci];
            let header_w = c.name().len();
            let data_w = (0..c.len())
                .map(|i| c.get(i).map(|v| format!("{v}").len()).unwrap_or(0))
                .max()
                .unwrap_or(0);
            header_w.max(data_w).max(MIN_COL_WIDTH)
        };

        // Budget: space after the row-number column and cursor_col's slot.
        let cursor_w = nat(cursor_col).min(max_col);
        let used_by_cursor = SP + cursor_w;
        let Some(mut budget) = inner_w
            .saturating_sub(row_num_w)
            .checked_sub(used_by_cursor)
        else {
            return cursor_col; // cursor alone doesn't fit — show it at left edge
        };

        // Walk left from cursor_col, fitting as many columns as possible.
        let mut offset = cursor_col;
        for ci in (0..cursor_col).rev() {
            let w = nat(ci).min(max_col);
            if budget < SP + w {
                break;
            }
            budget -= SP + w;
            offset = ci;
        }
        offset
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

    fn key(app: &mut App, code: KeyCode) {
        app.handle_key_event(KeyEvent::new(code, KeyModifiers::NONE))
            .unwrap();
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

    #[test]
    fn an_unknown_command_says_so() {
        let (mut app, _) = app_with("unknown.csv", SAMPLE);
        command(&mut app, "nope");
        assert_eq!(app.message.as_deref(), Some("not a command: :nope"));
        assert!(!app.exit);
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

    #[test]
    fn a_sorted_view_says_why_it_will_not_take_an_edit() {
        let (mut app, _) = app_with("sorted.csv", SAMPLE);
        if let Some(store) = &mut app.store {
            let _rx = store.begin_sort(0);
        }

        press(&mut app, 'i');
        assert!(matches!(app.mode, AppMode::Normal));
        let message = app.message.clone().unwrap();
        assert!(message.contains("sorted"), "{message}");
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
