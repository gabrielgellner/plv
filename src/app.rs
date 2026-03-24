use std::{io, path::PathBuf, sync::mpsc, time::Duration};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout},
    style::Style,
    widgets::Paragraph,
};

use crate::data::{loader, Store};
use crate::search::{SearchQuery, SearchState, SearchStatus};
use crate::ui::{DataTable, Prompt, StatusBar, Theme};

enum AppMode {
    Normal,
    Search,
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
    mode: AppMode,
    search_buf: String,
    search_state: Option<SearchState>,
    /// Receives batches of matching row indices from the background search thread.
    /// Dropping this cancels the search.
    search_rx: Option<mpsc::Receiver<Vec<usize>>>,
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
            mode: AppMode::Normal,
            search_buf: String::new(),
            search_state: None,
            search_rx: None,
        }
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
        if let Some(path) = self.file_path.clone() {
            let size = terminal.size()?;
            let vp = Self::viewport_rows(size.height);
            let result: anyhow::Result<Store> =
                loader::load(&path).and_then(|lf| Store::new(lf, vp));
            match result {
                Ok(store) => self.store = Some(store),
                Err(e) => self.error = Some(e.to_string()),
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

        let vp = Self::viewport_rows(area.height);
        if let Some(s) = &mut self.store
            && s.viewport_rows != vp
        {
            let _ = s.resize(vp);
        }

        let [table_area, status_area] = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .areas(area);

        let file_name = self
            .file_path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "plv".to_string());
        let col_offset = self.col_offset;
        let cursor_row = self.cursor_row;

        if let Some(store) = &self.store {
            frame.render_widget(
                DataTable {
                    df: &store.current_view,
                    col_offset,
                    row_offset: store.row_offset,
                    cursor_row,
                    theme: &self.theme,
                    search: self.search_state.as_ref(),
                },
                table_area,
            );

            match self.mode {
                AppMode::Search => {
                    frame.render_widget(
                        Prompt { buffer: &self.search_buf, theme: &self.theme },
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
                            pending_num: self.pending_num.clone(),
                            pending_z: self.pending_z,
                            theme: &self.theme,
                            search_info,
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
                Paragraph::new("Usage: plv <file.csv|file.parquet>").centered(),
                table_area,
            );
        }
    }

    fn handle_events(&mut self) -> io::Result<()> {
        // If poll_search changed any state (new matches, auto-jump, completion),
        // return immediately so the main loop redraws before blocking on input.
        if self.poll_search() {
            return Ok(());
        }

        // While a background search is running use a short timeout so the UI
        // redraws as result batches arrive. When idle, block on read directly.
        if self.search_rx.is_some() && !event::poll(Duration::from_millis(50))? {
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

    fn handle_key_event(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        self.message = None;
        match self.mode {
            AppMode::Search => self.handle_search_key(key),
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
                            let (tx, rx) = mpsc::channel();
                            store.search_async(query.raw.clone(), tx);
                            // Replace any in-progress search (dropping old rx cancels it)
                            self.search_rx = Some(rx);
                            self.search_state = Some(SearchState::new(query));
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
            KeyCode::Char('q') | KeyCode::Char('Q') => self.exit = true,

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
                    let last =
                        self.store.as_ref().map_or(0, |s| s.total_rows.saturating_sub(1));
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
                self.col_offset = self.col_offset.saturating_sub(1);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.pending_num.clear();
                if let Some(store) = &self.store {
                    let max_col = store.schema.len().saturating_sub(1);
                    if self.col_offset < max_col {
                        self.col_offset += 1;
                    }
                }
            }
            KeyCode::Char('H') => {
                self.pending_num.clear();
                self.col_offset = 0;
            }

            // Search
            KeyCode::Char('/') => {
                self.pending_num.clear();
                self.search_buf.clear();
                self.mode = AppMode::Search;
            }
            KeyCode::Char('n') => {
                self.pending_num.clear();
                match &mut self.search_state {
                    None => {}
                    Some(state) if state.matching_rows.is_empty() => {
                        self.message = Some("Searching…".to_string());
                    }
                    Some(state) => {
                        if let Some(row) = state.next_from(self.cursor_row) {
                            self.cursor_to(row)?;
                        }
                    }
                }
            }
            KeyCode::Char('N') => {
                self.pending_num.clear();
                match &mut self.search_state {
                    None => {}
                    Some(state) if state.matching_rows.is_empty() => {
                        self.message = Some("Searching…".to_string());
                    }
                    Some(state) => {
                        if let Some(row) = state.prev_from(self.cursor_row) {
                            self.cursor_to(row)?;
                        }
                    }
                }
            }
            KeyCode::Esc => {
                self.search_state = None;
                self.search_rx = None; // dropping rx cancels background scan
                self.pending_num.clear();
            }

            _ => {
                self.pending_num.clear();
            }
        }
        Ok(())
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
}
