use std::{io, path::PathBuf};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout},
    style::Style,
    widgets::Paragraph,
};

use crate::data::{loader, Store};
use crate::ui::{DataTable, StatusBar};

pub struct App {
    file_path: Option<PathBuf>,
    store: Option<Store>,
    col_offset: usize,
    exit: bool,
    error: Option<String>,
}

impl App {
    pub fn new(file: Option<PathBuf>) -> Self {
        Self {
            file_path: file,
            store: None,
            col_offset: 0,
            exit: false,
            error: None,
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

        // Handle terminal resize
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

        // Extract what we need before borrowing store
        let file_name = self.file_path.as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "plv".to_string());
        let col_offset = self.col_offset;

        if let Some(store) = &self.store {
            frame.render_widget(
                DataTable {
                    df: &store.current_view,
                    col_offset,
                    row_offset: store.row_offset,
                    title: &file_name,
                },
                table_area,
            );
            frame.render_widget(
                StatusBar {
                    file_name,
                    row_offset: store.row_offset,
                    total_rows: store.total_rows,
                    col_offset,
                    total_cols: store.schema.len(),
                    viewport_rows: store.viewport_rows,
                },
                status_area,
            );
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
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && let Err(e) = self.handle_key_event(key)
        {
            self.error = Some(e.to_string());
        }
        Ok(())
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => self.exit = true,

            // Row scrolling
            KeyCode::Char('j') | KeyCode::Down => self.with_store(|s| s.scroll_down(1))?,
            KeyCode::Char('k') | KeyCode::Up => self.with_store(|s| s.scroll_up(1))?,
            KeyCode::Char('d') if ctrl => self.with_store(|s| {
                let n = (s.viewport_rows / 2).max(1);
                s.scroll_down(n)
            })?,
            KeyCode::Char('u') if ctrl => self.with_store(|s| {
                let n = (s.viewport_rows / 2).max(1);
                s.scroll_up(n)
            })?,
            KeyCode::Char('g') | KeyCode::Home => self.with_store(|s| s.scroll_to_top())?,
            KeyCode::Char('G') | KeyCode::End => self.with_store(|s| s.scroll_to_bottom())?,

            // Column scrolling
            KeyCode::Char('h') | KeyCode::Left => {
                self.col_offset = self.col_offset.saturating_sub(1);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                if let Some(store) = &self.store {
                    let max_col = store.schema.len().saturating_sub(1);
                    if self.col_offset < max_col {
                        self.col_offset += 1;
                    }
                }
            }
            KeyCode::Char('H') => self.col_offset = 0,

            _ => {}
        }
        Ok(())
    }

    fn with_store<F>(&mut self, f: F) -> anyhow::Result<()>
    where
        F: FnOnce(&mut Store) -> anyhow::Result<()>,
    {
        if let Some(store) = &mut self.store {
            f(store)?;
        }
        Ok(())
    }
}
