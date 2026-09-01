use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::{self, yield_now};

use anyhow::{Result, bail};
use duckdb::Connection;
use polars::prelude::*;

use crate::data::edit::{Cell, Overlay};
use crate::data::lake_db::{self, LakeSource};
use crate::data::loader;
use crate::data::rows::{self, RowSet};
use crate::data::writer::{self, Stamp};
use crate::view::View;

/// Where a store's rows come from.
///
/// CSV and Parquet are read lazily by Polars. Lake tables go through DuckDB's
/// `ducklake` extension instead, so that inlined rows, delete files and schema
/// evolution are handled by the format's own reader rather than by us.
enum Source {
    Lazy(LazyFrame),
    Lake(LakeQuery),
}

struct LakeQuery {
    conn: Connection,
    source: LakeSource,
    columns: Vec<String>,
}

/// A file plv can write edits back to.
struct EditTarget {
    path: PathBuf,
    separator: u8,
    /// How the file looked when it was opened. A write checks this first, so
    /// it cannot clobber changes something else made in the meantime — the
    /// buffer's row numbers describe the file as it was read.
    stamp: Stamp,
}

pub struct Store {
    source: Source,
    pub schema: SchemaRef,
    pub total_rows: usize,
    pub row_offset: usize,
    pub viewport_rows: usize,
    pub current_view: DataFrame,
    /// What the viewer is showing: which columns, in what order, sorted how.
    ///
    /// Its indices are **source** columns, matching `schema`. Everything above
    /// this layer counts in *display* positions instead, and `Store` converts
    /// at its own boundary — see [`Store::source_column`].
    pub view: View,
    /// Present when the rows came from a delimited text file, which is the
    /// only kind plv can write back.
    edit: Option<EditTarget>,
    /// Edits made but not yet written, keyed by position in the source file.
    overlay: Overlay,
    /// The rows a `:filter` matched, when one is active. Present means the
    /// viewer is paging through this set rather than through the file.
    filter_rows: Option<RowSet>,
}

impl Store {
    /// Open a store over a Polars frame, counting rows by scanning. Prefer
    /// [`Store::with_row_count`] when the row count is already known.
    pub fn new(lf: LazyFrame, viewport_rows: usize) -> Result<Self> {
        Self::build(lf, viewport_rows, None)
    }

    /// Open a store over a Polars frame with a caller-supplied row count.
    pub fn with_row_count(lf: LazyFrame, viewport_rows: usize, total_rows: usize) -> Result<Self> {
        Self::build(lf, viewport_rows, Some(total_rows))
    }

    fn build(mut lf: LazyFrame, viewport_rows: usize, total_rows: Option<usize>) -> Result<Self> {
        let schema = lf.collect_schema()?;
        let total_rows = match total_rows {
            Some(n) => n,
            None => Self::count_rows(&lf)?,
        };
        let current_view = Self::fetch_lazy(&lf, 0, viewport_rows)?;
        Ok(Self {
            source: Source::Lazy(lf),
            schema,
            total_rows,
            row_offset: 0,
            viewport_rows,
            current_view,
            view: View::default(),
            edit: None,
            overlay: Overlay::new(),
            filter_rows: None,
        })
    }

    /// Open a store over a file, remembering the path so edits can be written
    /// back to it.
    pub fn open_file(path: &Path, viewport_rows: usize) -> Result<Self> {
        let mut store = Self::build(loader::load(path)?, viewport_rows, None)?;
        // Only delimited text is editable. Parquet is genuinely typed, so a
        // one-cell change would mean rewriting the whole file against a schema.
        if let Some(separator) = loader::separator(path)? {
            store.edit = Some(EditTarget {
                path: path.to_path_buf(),
                separator,
                stamp: Stamp::of(path)?,
            });
        }
        Ok(store)
    }

    /// Open a store over one table or partition of a lake.
    ///
    /// `total_rows` comes from `count(*)`, which the extension answers from
    /// catalog statistics — a metadata lookup even on a billion-row table.
    pub fn new_lake(
        conn: Connection,
        source: LakeSource,
        viewport_rows: usize,
        total_rows: usize,
    ) -> Result<Self> {
        let columns = lake_db::column_names(&conn, &source)?;
        let query = LakeQuery {
            conn,
            source,
            columns,
        };
        let current_view = lake_db::page_with(&query.conn, &query.source, &[], 0, viewport_rows)?;

        // Take the schema from the first page: it is the only place column
        // types are observable, and the viewer only needs names and arity.
        let schema = current_view.schema().clone();
        Ok(Self {
            source: Source::Lake(query),
            schema,
            total_rows,
            row_offset: 0,
            viewport_rows,
            current_view,
            view: View::default(),
            edit: None,
            overlay: Overlay::new(),
            filter_rows: None,
        })
    }

    // ── rows: a filter replaces the row space ────────────────────────────

    /// Rows on show. Not `total_rows`, which stays the file's own count —
    /// the writer needs that to check it is looking at the same file.
    pub fn row_count(&self) -> usize {
        match &self.filter_rows {
            Some(set) => set.len(),
            None => self.total_rows,
        }
    }

    /// The source row behind a display position.
    pub fn source_row(&self, display: usize) -> Option<usize> {
        match &self.filter_rows {
            Some(set) => set.source(display),
            None => (display < self.total_rows).then_some(display),
        }
    }

    /// Where a source row appears, if it survived the filter.
    pub fn display_row(&self, source: usize) -> Option<usize> {
        match &self.filter_rows {
            Some(set) => set.display(source),
            None => (source < self.total_rows).then_some(source),
        }
    }

    /// Whether a filter is still scanning, for the spinner.
    pub fn filtering(&self) -> bool {
        self.filter_rows
            .as_ref()
            .is_some_and(|set| !set.is_complete())
    }

    // ── columns: source indices below, display positions above ───────────

    /// Source column indices, in the order they are shown.
    pub fn columns(&self) -> Vec<usize> {
        self.view.columns(self.schema.len())
    }

    /// How many columns are on show. Not `schema.len()` once a view narrows
    /// the frame — that is the file's column count, a different question.
    pub fn column_count(&self) -> usize {
        self.view
            .select
            .as_ref()
            .map_or_else(|| self.schema.len(), Vec::len)
    }

    /// The source column behind a display position.
    pub fn source_column(&self, display: usize) -> Option<usize> {
        match &self.view.select {
            Some(cols) => cols.get(display).copied(),
            None => (display < self.schema.len()).then_some(display),
        }
    }

    /// Where a source column appears, if it is on show at all.
    pub fn display_column(&self, source: usize) -> Option<usize> {
        match &self.view.select {
            Some(cols) => cols.iter().position(|&c| c == source),
            None => (source < self.schema.len()).then_some(source),
        }
    }

    /// Name and type of the column at a display position.
    pub fn column_info(&self, display: usize) -> Option<(String, DataType)> {
        let source = self.source_column(display)?;
        self.schema
            .get_at_index(source)
            .map(|(name, dtype)| (name.to_string(), dtype.clone()))
    }

    /// Sort keys as display positions, for the header indicators. A key on a
    /// column the view has hidden simply does not appear.
    pub fn sort_display(&self) -> Vec<(usize, bool)> {
        self.view
            .sort
            .iter()
            .filter_map(|&(source, asc)| self.display_column(source).map(|d| (d, asc)))
            .collect()
    }

    /// Sort keys as `(column_name, ascending)`, dropping any stale indices.
    fn sort_keys(&self) -> Vec<(String, bool)> {
        self.view
            .sort
            .iter()
            .filter_map(|&(ci, asc)| {
                self.schema
                    .get_at_index(ci)
                    .map(|(name, _)| (name.to_string(), asc))
            })
            .collect()
    }

    /// The frame the viewer actually reads: the base with the view composed
    /// onto it.
    ///
    /// The order is fixed and does not follow the order the commands were
    /// typed — **sort, then projection**, as in SQL — so a sort can name a
    /// column the view is not showing. Filtering will join the front of the
    /// same pipeline, but through a row-index set rather than here: a filter
    /// stops Polars pushing the slice down into the scan, which would turn
    /// every keypress into a full read of the file.
    fn effective_lf(&self) -> Option<LazyFrame> {
        let Source::Lazy(base) = &self.source else {
            return None;
        };
        let mut lf = base.clone();

        let keys = self.sort_keys();
        if !keys.is_empty() {
            let (names, descending): (Vec<String>, Vec<bool>) =
                keys.into_iter().map(|(name, asc)| (name, !asc)).unzip();
            lf = lf.sort(
                names,
                SortMultipleOptions::default().with_order_descending_multi(descending),
            );
        }

        if self.view.select.is_some() {
            let shown: Vec<Expr> = self
                .columns()
                .into_iter()
                .filter_map(|source| self.schema.get_at_index(source))
                .map(|(name, _)| col(name.as_str()))
                .collect();
            if !shown.is_empty() {
                lf = lf.select(shown);
            }
        }
        Some(lf)
    }

    /// Adopt a new view, keeping the old one if the new one will not collect.
    ///
    /// The command was already checked against the schema when it was typed;
    /// what can still fail is the frame. A viewer showing an error instead of
    /// data because of one mistyped command would be worse than a refusal.
    pub fn apply_view(&mut self, view: View) -> Result<()> {
        let previous = std::mem::replace(&mut self.view, view);
        match self.fetch(self.row_offset, self.viewport_rows) {
            Ok(df) => {
                self.current_view = df;
                Ok(())
            }
            Err(e) => {
                self.view = previous;
                Err(e)
            }
        }
    }

    fn fetch(&self, offset: usize, height: usize) -> Result<DataFrame> {
        let df = match &self.source {
            Source::Lazy(_) => {
                let lf = self.effective_lf().expect("lazy source");
                match &self.filter_rows {
                    Some(set) => Self::gather(&lf, set, offset, height),
                    None => Self::fetch_lazy(&lf, offset, height),
                }
            }
            Source::Lake(query) => lake_db::page_with(
                &query.conn,
                &query.source,
                &self.sort_keys(),
                offset,
                height,
            ),
        }?;
        self.apply_overlay(df, offset)
    }

    /// A page picked out of the frame by row index.
    ///
    /// Reads the span the page covers and takes the wanted rows from it. The
    /// span is the unavoidable part — those rows have to be read — and Polars
    /// still pushes that slice into the scan, so it costs what scrolling to
    /// the same point unfiltered would.
    fn gather(lf: &LazyFrame, set: &RowSet, offset: usize, height: usize) -> Result<DataFrame> {
        let page = set.page(offset, height);
        let (Some(&first), Some(&last)) = (page.first(), page.last()) else {
            // No rows, but the caller still needs the right columns.
            return Ok(lf.clone().slice(0, 0).collect()?);
        };
        let df = lf
            .clone()
            .slice(first as i64, (last - first + 1) as u32)
            .collect()?;
        let wanted: Vec<IdxSize> = page.iter().map(|&row| (row - first) as IdxSize).collect();
        Ok(df.take(&IdxCa::from_vec(PlSmallStr::from_static("i"), wanted))?)
    }

    /// Toggle sort direction on `col_idx`, or add it as a new ascending sort key.
    /// Updates sort state immediately and spawns a background thread to fetch
    /// the new first page. The caller should replace `current_view` when the
    /// DataFrame arrives on the returned receiver.
    pub fn begin_sort(&mut self, display_col: usize) -> mpsc::Receiver<DataFrame> {
        let (tx, rx) = mpsc::channel();
        let Some(source_col) = self.source_column(display_col) else {
            return rx;
        };
        if let Some(entry) = self.view.sort.iter_mut().find(|(ci, _)| *ci == source_col) {
            entry.1 = !entry.1;
        } else {
            self.view.sort.push((source_col, true));
        }
        self.row_offset = 0;
        let vp = self.viewport_rows;

        match &self.source {
            Source::Lazy(_) => {
                let lf = self.effective_lf().expect("lazy source");
                thread::spawn(move || {
                    if let Ok(df) = Self::fetch_lazy(&lf, 0, vp) {
                        let _ = tx.send(df);
                    }
                });
            }
            Source::Lake(query) => {
                // A cloned handle shares the attached lake, so the background
                // thread does not pay the ATTACH cost again.
                let Ok(conn) = query.conn.try_clone() else {
                    return rx;
                };
                let source = query.source.clone();
                let keys = self.sort_keys();
                thread::spawn(move || {
                    if let Ok(df) = lake_db::page_with(&conn, &source, &keys, 0, vp) {
                        let _ = tx.send(df);
                    }
                });
            }
        }
        rx
    }

    /// Clear all sort keys and return to natural order.
    pub fn clear_sort(&mut self) -> Result<()> {
        self.view.sort.clear();
        self.row_offset = 0;
        self.current_view = self.fetch(0, self.viewport_rows)?;
        Ok(())
    }

    pub fn scroll_to_offset(&mut self, offset: usize) -> Result<()> {
        let max = self.row_count().saturating_sub(self.viewport_rows);
        self.row_offset = offset.min(max);
        self.current_view = self.fetch(self.row_offset, self.viewport_rows)?;
        Ok(())
    }

    pub fn resize(&mut self, new_height: usize) -> Result<()> {
        if self.viewport_rows != new_height && new_height > 0 {
            self.viewport_rows = new_height;
            self.current_view = self.fetch(self.row_offset, self.viewport_rows)?;
        }
        Ok(())
    }

    /// Replace the visible page with a frame produced off the main thread,
    /// re-applying pending edits so a background fetch cannot drop them.
    pub fn set_view(&mut self, df: DataFrame) {
        if let Ok(df) = self.apply_overlay(df, self.row_offset) {
            self.current_view = df;
        }
    }

    /// Why this store cannot take an edit right now, or `None` when it can.
    ///
    /// The message lives here rather than in the key handler so the rule and
    /// its explanation cannot drift apart.
    pub fn edit_blocked(&self) -> Option<&'static str> {
        if self.edit.is_none() {
            return Some(match self.source {
                Source::Lake(_) => "lake tables are read-only",
                Source::Lazy(_) => "only csv, tsv, tab and txt files can be edited",
            });
        }
        if !self.view.sort.is_empty() {
            // A sorted page's rows are not the file's rows, so an edit could
            // not be told which line it belongs to.
            return Some("cannot edit a sorted view — clear the sort first");
        }
        None
    }

    /// Whether the rows came from a file plv can write back to.
    ///
    /// Unlike [`Store::edit_blocked`] this does not change with the sort, so it
    /// is the right question for deciding what to advertise in the help.
    pub fn is_editable(&self) -> bool {
        self.edit.is_some()
    }

    /// The visible text of one cell, by absolute row and column index.
    ///
    /// `None` when the row is not on the current page. A null cell reads as
    /// empty, which is also how an empty field is written back.
    pub fn cell_text(&self, row: usize, col: usize) -> Option<String> {
        let local = row.checked_sub(self.row_offset)?;
        if local >= self.current_view.height() {
            return None;
        }
        let column = self.current_view.columns().get(col)?;
        let text = column.cast(&DataType::String).ok()?;
        Some(text.str().ok()?.get(local).unwrap_or("").to_string())
    }

    /// Cells holding an edit that has not been written yet.
    pub fn dirty(&self) -> usize {
        self.overlay.len()
    }

    /// Apply `edits` as a single undoable change. Both coordinates are
    /// display positions.
    pub fn edit<I: IntoIterator<Item = (Cell, String)>>(&mut self, edits: I) -> Result<()> {
        // Cells arrive in display coordinates, as everything above this layer
        // counts them, and are stored against source columns — so an edit made
        // through a narrowed or reordered view still lands on the right field
        // of the file.
        let mapped: Vec<(Cell, String)> = edits
            .into_iter()
            .filter_map(|((row, display), value)| {
                let cell = (self.source_row(row)?, self.source_column(display)?);
                Some((cell, value))
            })
            .collect();
        self.overlay.set(mapped);
        self.refresh()
    }

    pub fn undo(&mut self) -> Result<bool> {
        let undone = self.overlay.undo();
        if undone {
            self.refresh()?;
        }
        Ok(undone)
    }

    pub fn redo(&mut self) -> Result<bool> {
        let redone = self.overlay.redo();
        if redone {
            self.refresh()?;
        }
        Ok(redone)
    }

    /// Write pending edits back to the source file, or to `dst` for `:w path`.
    ///
    /// Refuses when the file has changed since it was opened unless `force`:
    /// the buffer's row numbers describe the file as it was read, so writing
    /// over a different one would put edits on the wrong lines.
    ///
    /// Writing elsewhere leaves the buffer dirty, as `:w path` does in vim —
    /// the source file still lacks these changes.
    pub fn save(&mut self, dst: Option<&Path>, force: bool) -> Result<PathBuf> {
        let Some(target) = &self.edit else {
            bail!("this view is read-only");
        };
        if !force && !target.stamp.still_matches(&target.path) {
            bail!(
                "{} has changed on disk — :w! overwrites it",
                target.path.display()
            );
        }
        let src = target.path.clone();
        let separator = target.separator;
        let dst = dst.unwrap_or(&src).to_path_buf();

        // plv always reads a header row, so record 0 is never data.
        let stamp = writer::save(&src, &dst, separator, true, &self.overlay, self.total_rows)?;

        if dst == src {
            self.overlay.clear();
            if let Some(target) = &mut self.edit {
                target.stamp = stamp;
            }
            self.reload()?;
        }
        Ok(dst)
    }

    /// Re-open the file after writing to it, in case an edit changed a
    /// column's inferred type. The row count cannot have changed: a value
    /// containing a newline is quoted, so it stays one record.
    fn reload(&mut self) -> Result<()> {
        let Some(path) = self.edit.as_ref().map(|t| t.path.clone()) else {
            return Ok(());
        };
        let mut lf = loader::load(&path)?;
        self.schema = lf.collect_schema()?;
        self.source = Source::Lazy(lf);
        self.refresh()
    }

    fn refresh(&mut self) -> Result<()> {
        self.current_view = self.fetch(self.row_offset, self.viewport_rows)?;
        Ok(())
    }

    /// Paint pending edits onto a freshly fetched page.
    ///
    /// A column carrying an edit is rendered as text, so the value is shown
    /// exactly as it was typed whether or not it still parses as the column's
    /// inferred type — the file itself is untyped, and pretending otherwise
    /// would hide what is about to be written. Only the visible rows are
    /// touched, so the cost follows the viewport and not the file.
    fn apply_overlay(&self, mut df: DataFrame, offset: usize) -> Result<DataFrame> {
        if self.overlay.is_empty() {
            return Ok(df);
        }
        let height = df.height();

        // Keyed by display position: the overlay stores source columns, and a
        // view can reorder them, hide them, or both.
        let mut by_column: BTreeMap<usize, Vec<(usize, &str)>> = BTreeMap::new();
        for (local, cells) in self.edits_in_page(offset, height) {
            for (&source, value) in cells {
                if let Some(display) = self.display_column(source) {
                    by_column
                        .entry(display)
                        .or_default()
                        .push((local, value.as_str()));
                }
            }
        }

        for (index, edits) in by_column {
            let Some(column) = df.columns().get(index) else {
                continue;
            };
            let patched = Self::patch_column(column, &edits)?;
            df.with_column(patched)?;
        }
        Ok(df)
    }

    /// One column with its pending edits written in.
    ///
    /// The values are text, so the column has to go through text to take them.
    /// It comes back if it can: typing `42` into a number is still a number,
    /// and letting one edit turn the whole column into strings would change how
    /// every other value in it is aligned and formatted. Only a value that
    /// genuinely does not fit leaves the column as text — which is the honest
    /// answer, because that is what the file will read as next time.
    fn patch_column(column: &Column, edits: &[(usize, &str)]) -> Result<Column> {
        let name = column.name().clone();
        let dtype = column.dtype().clone();

        let text = column.cast(&DataType::String)?;
        let mut values: Vec<Option<String>> = text
            .str()?
            .into_iter()
            .map(|v| v.map(str::to_string))
            .collect();
        for &(row, value) in edits {
            // An empty field is a null, matching how it is read and written.
            values[row] = (!value.is_empty()).then(|| value.to_string());
        }

        let patched = Column::new(name, values);
        // Strict: a plain `cast` turns a value it cannot parse into a null,
        // which would quietly swallow the edit instead of showing it.
        match patched.strict_cast(&dtype) {
            Ok(typed) => Ok(typed),
            Err(_) => Ok(patched),
        }
    }

    /// Pending edits falling inside a page of `height` rows starting at
    /// `offset`, paired with the row's position within it.
    ///
    /// One window for every reader — the renderer marking edited cells and the
    /// overlay painting them — so the two cannot disagree about which rows are
    /// on screen.
    fn edits_in_page(
        &self,
        offset: usize,
        height: usize,
    ) -> impl Iterator<Item = (usize, &BTreeMap<usize, String>)> {
        // Every edit is considered rather than the run between two offsets: a
        // filter can drop rows from between them, so "past the page" is no
        // longer something the source row number can be asked directly. There
        // are few edits and one page, so this is cheap either way.
        self.overlay.rows().filter_map(move |(source, cells)| {
            let local = self.display_row(source)?.checked_sub(offset)?;
            (local < height).then_some((local, cells))
        })
    }

    /// The displayed text of every cell in a block, pending edits included.
    ///
    /// Unlike [`Store::cell_text`] this is not limited to the visible page: a
    /// selection can be taller than the viewport, and an operator that builds
    /// on what is already in each cell has to see all of it. Indexed by
    /// position within the block, and bounded by the caller — this materializes
    /// every row it covers.
    pub fn block_text(
        &self,
        rows: (usize, usize),
        cols: (usize, usize),
    ) -> Result<Vec<Vec<String>>> {
        let height = rows.1.saturating_sub(rows.0) + 1;
        let df = self.fetch(rows.0, height)?;

        let columns: Vec<Option<Column>> = (cols.0..=cols.1)
            .map(|index| {
                df.columns()
                    .get(index)
                    .and_then(|c| c.cast(&DataType::String).ok())
            })
            .collect();

        Ok((0..df.height())
            .map(|row| {
                columns
                    .iter()
                    .map(|column| {
                        column
                            .as_ref()
                            .and_then(|c| c.str().ok()?.get(row))
                            .unwrap_or("")
                            .to_string()
                    })
                    .collect()
            })
            .collect())
    }

    /// Pending edits inside the current page, as `(row within the page,
    /// display position)` — what the table needs in order to mark them. Edits
    /// on a column the view has hidden are not reported: there is nowhere on
    /// screen to report them.
    pub fn edited_cells(&self) -> Vec<(usize, usize)> {
        self.edits_in_page(self.row_offset, self.current_view.height())
            .flat_map(|(local, cells)| {
                cells
                    .keys()
                    .filter_map(move |&source| Some((local, self.display_column(source)?)))
            })
            .collect()
    }

    fn fetch_lazy(lf: &LazyFrame, offset: usize, height: usize) -> Result<DataFrame> {
        Ok(lf.clone().slice(offset as i64, height as u32).collect()?)
    }

    /// Spawn a background thread that scans for `pattern` (regex) in chunks,
    /// sending batches of matching absolute row indices down `tx`.
    ///
    /// The caller drops `tx`'s paired `Receiver` to cancel early — the thread
    /// will notice the send failure and exit cleanly.
    pub fn search_async(
        &self,
        pattern: String,
        col_name: Option<String>,
        tx: mpsc::Sender<Vec<usize>>,
    ) {
        match &self.source {
            Source::Lazy(_) => self.search_lazy(pattern, col_name, tx),
            Source::Lake(query) => self.search_lake(query, pattern, col_name, tx),
        }
    }

    fn search_lake(
        &self,
        query: &LakeQuery,
        pattern: String,
        col_name: Option<String>,
        tx: mpsc::Sender<Vec<usize>>,
    ) {
        let Ok(conn) = query.conn.try_clone() else {
            return;
        };
        let source = query.source.clone();
        let columns = query.columns.clone();
        let keys = self.sort_keys();
        let total = self.total_rows;

        thread::spawn(move || {
            const CHUNK: usize = 10_000;
            let mut offset = 0usize;

            while offset < total {
                let size = CHUNK.min(total - offset);
                let sql = lake_db::match_indices_sql(
                    &source,
                    &keys,
                    col_name.as_deref(),
                    &pattern,
                    offset,
                    size,
                    &columns,
                );

                let Ok(mut stmt) = conn.prepare(&sql) else {
                    break;
                };
                let Ok(mut rows) = stmt.query([]) else { break };

                let mut batch = Vec::new();
                loop {
                    match rows.next() {
                        Ok(Some(row)) => match row.get::<_, i64>(0) {
                            Ok(idx) if idx >= 0 => batch.push(idx as usize),
                            _ => {}
                        },
                        Ok(None) => break,
                        Err(_) => return,
                    }
                }

                if !batch.is_empty() && tx.send(batch).is_err() {
                    return; // receiver dropped — search cancelled
                }
                offset += CHUNK;
            }
        });
    }

    fn search_lazy(&self, pattern: String, col_name: Option<String>, tx: mpsc::Sender<Vec<usize>>) {
        let Some(lf) = self.effective_lf() else {
            return;
        };
        let schema = self.schema.clone();
        Self::scan_rows(lf, self.total_rows, tx, move || {
            // Built inside the thread: an `Expr` is not `Send`.
            match col_name {
                Some(name) => Some(matches_pattern(&name, &pattern)),
                None => schema
                    .iter_names()
                    .map(|name| matches_pattern(name.as_str(), &pattern))
                    .reduce(Expr::or),
            }
        });
    }

    /// Scan `lf` in chunks, streaming the absolute indices of the rows an
    /// expression keeps.
    ///
    /// Shared by `/` search and `:filter`: both ask the same question of the
    /// file, and both want the answer progressively rather than all at once,
    /// because on a large file "all at once" means a frozen viewer.
    ///
    /// `build` runs on the worker thread — `Expr` is not `Send`, so the
    /// expression cannot be handed across. Dropping the receiver cancels the
    /// scan; the thread notices on its next send.
    fn scan_rows<F>(lf: LazyFrame, total: usize, tx: mpsc::Sender<Vec<usize>>, build: F)
    where
        F: FnOnce() -> Option<Expr> + Send + 'static,
    {
        thread::spawn(move || {
            let Some(predicate) = build() else { return };

            const CHUNK: usize = 10_000;
            let mut offset = 0usize;

            while offset < total {
                let size = CHUNK.min(total - offset);

                let Ok(df) = lf
                    .clone()
                    .slice(offset as i64, size as u32)
                    .with_row_index("__idx__", Some(offset as u32))
                    .filter(predicate.clone())
                    .select([col("__idx__")])
                    .collect()
                else {
                    break;
                };

                let rows: Vec<usize> = df
                    .column("__idx__")
                    .ok()
                    .and_then(|c| c.u32().ok())
                    .map(|ca| ca.into_iter().flatten().map(|i| i as usize).collect())
                    .unwrap_or_default();

                if !rows.is_empty() && tx.send(rows).is_err() {
                    return; // receiver dropped — cancelled
                }

                offset += CHUNK;

                // Yield between chunks so the main thread's scroll queries can
                // interleave without lag.
                yield_now();
            }
        });
    }

    /// Bring the filtered row set in line with the view.
    ///
    /// Returns a receiver of matching-row batches when a scan has started.
    /// `None` means there is nothing to filter by and the whole file is on
    /// show again.
    pub fn begin_filter(&mut self) -> Result<Option<mpsc::Receiver<Vec<usize>>>> {
        let Some(filter) = self.view.filter.clone() else {
            self.filter_rows = None;
            self.row_offset = 0;
            self.refresh()?;
            return Ok(None);
        };
        let Source::Lazy(base) = &self.source else {
            return Ok(None);
        };

        // The scan runs on the base frame, so the indices it reports are the
        // file's own rows. That is what makes them usable as edit-buffer keys.
        let lf = base.clone();
        let schema = self.schema.clone();
        let (tx, rx) = mpsc::channel();

        self.filter_rows = Some(RowSet::new());
        self.row_offset = 0;
        self.refresh()?;

        Self::scan_rows(lf, self.total_rows, tx, move || {
            rows::predicate(&filter, &schema)
        });
        Ok(Some(rx))
    }

    /// Take a batch of matching rows from the scan.
    pub fn extend_filter(&mut self, batch: Vec<usize>) -> Result<()> {
        if let Some(set) = &mut self.filter_rows {
            set.extend(batch);
        }
        self.refresh()
    }

    /// The scan has run out; what is here is all of it.
    pub fn finish_filter(&mut self) -> Result<()> {
        if let Some(set) = &mut self.filter_rows {
            set.finish();
        }
        self.refresh()
    }

    fn count_rows(lf: &LazyFrame) -> Result<usize> {
        let df = lf.clone().select([len().alias("n")]).collect()?;
        Ok(df.column("n")?.u32()?.get(0).unwrap_or(0) as usize)
    }
}

/// A column rendered as text and matched against a pattern — how `/` search
/// reads every column, and how `~` reads one.
fn matches_pattern(column: &str, pattern: &str) -> Expr {
    col(column)
        .cast(DataType::String)
        .str()
        .contains(lit(pattern), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(name: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("plv-store-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// A cell of the visible page, read as text whatever its column's type.
    fn cell(df: &DataFrame, col: usize, row: usize) -> Option<String> {
        let column = df.columns().get(col)?.cast(&DataType::String).ok()?;
        column.str().ok()?.get(row).map(str::to_string)
    }

    const SAMPLE: &str = "name,count\na,1\nb,2\nc,3\nd,4\n";

    #[test]
    fn an_edit_shows_in_the_page() {
        let path = write_temp("shows.csv", SAMPLE);
        let mut store = Store::open_file(&path, 10).unwrap();
        assert_eq!(cell(&store.current_view, 0, 1).as_deref(), Some("b"));

        store.edit([((1, 0), "edited".to_string())]).unwrap();
        assert_eq!(cell(&store.current_view, 0, 1).as_deref(), Some("edited"));
        assert_eq!(store.dirty(), 1);
        // Its neighbours are untouched.
        assert_eq!(cell(&store.current_view, 0, 0).as_deref(), Some("a"));
        assert_eq!(cell(&store.current_view, 1, 1).as_deref(), Some("2"));
    }

    #[test]
    fn an_edit_is_keyed_to_the_file_and_not_the_viewport() {
        let path = write_temp("scroll.csv", SAMPLE);
        let mut store = Store::open_file(&path, 2).unwrap();
        store.edit([((3, 0), "far".to_string())]).unwrap();

        // Row 3 is off the page, so nothing in view changed.
        assert_eq!(cell(&store.current_view, 0, 0).as_deref(), Some("a"));
        assert_eq!(cell(&store.current_view, 0, 1).as_deref(), Some("b"));

        // Scrolling to it finds the edit waiting.
        store.scroll_to_offset(2).unwrap();
        assert_eq!(cell(&store.current_view, 0, 0).as_deref(), Some("c"));
        assert_eq!(cell(&store.current_view, 0, 1).as_deref(), Some("far"));
    }

    #[test]
    fn a_value_that_does_not_fit_the_column_type_is_still_shown_as_typed() {
        let path = write_temp("types.csv", SAMPLE);
        let mut store = Store::open_file(&path, 10).unwrap();
        assert!(matches!(
            store.schema.get_at_index(1).unwrap().1,
            DataType::Int64
        ));

        store.edit([((0, 1), "n/a".to_string())]).unwrap();
        assert_eq!(cell(&store.current_view, 1, 0).as_deref(), Some("n/a"));
        // The rest of the column survives the switch to text.
        assert_eq!(cell(&store.current_view, 1, 1).as_deref(), Some("2"));
    }

    #[test]
    fn an_edit_that_still_reads_as_a_number_keeps_the_column_numeric() {
        let path = write_temp("keeptype.csv", SAMPLE);
        let mut store = Store::open_file(&path, 10).unwrap();

        store.edit([((1, 1), "42".to_string())]).unwrap();
        assert_eq!(cell(&store.current_view, 1, 1).as_deref(), Some("42"));
        assert_eq!(
            store.current_view.columns()[1].dtype(),
            &DataType::Int64,
            "one edit must not turn the whole column into text"
        );
    }

    #[test]
    fn an_edit_that_does_not_read_as_a_number_leaves_the_column_as_text() {
        let path = write_temp("losetype.csv", SAMPLE);
        let mut store = Store::open_file(&path, 10).unwrap();

        store.edit([((1, 1), "n/a".to_string())]).unwrap();
        assert_eq!(cell(&store.current_view, 1, 1).as_deref(), Some("n/a"));
        assert_eq!(
            store.current_view.columns()[1].dtype(),
            &DataType::String,
            "the column really will read as text next time it is opened"
        );
    }

    #[test]
    fn clearing_a_numeric_cell_leaves_a_null_rather_than_text() {
        let path = write_temp("clearnum.csv", SAMPLE);
        let mut store = Store::open_file(&path, 10).unwrap();

        store.edit([((0, 1), String::new())]).unwrap();
        assert_eq!(store.current_view.columns()[1].dtype(), &DataType::Int64);
        assert!(
            store.current_view.columns()[1].get(0).unwrap().is_null(),
            "an empty field is a null, as it is on the way back out"
        );
    }

    #[test]
    fn untouched_values_in_an_edited_column_are_unchanged() {
        // The column goes through text to take the edit, so the values that
        // were not edited have to survive the round trip exactly.
        let path = write_temp(
            "floats.csv",
            "name,ratio\na,0.1\nb,3.14159265358979\nc,2.5\n",
        );
        let mut store = Store::open_file(&path, 10).unwrap();
        let before: Vec<Option<String>> = (0..3).map(|r| cell(&store.current_view, 1, r)).collect();

        store.edit([((0, 1), "9.5".to_string())]).unwrap();
        assert_eq!(store.current_view.columns()[1].dtype(), &DataType::Float64);
        assert_eq!(cell(&store.current_view, 1, 0).as_deref(), Some("9.5"));
        for row in 1..3 {
            assert_eq!(cell(&store.current_view, 1, row), before[row], "row {row}");
        }
    }

    #[test]
    fn edited_cells_are_reported_relative_to_the_page() {
        let path = write_temp("marks.csv", SAMPLE);
        let mut store = Store::open_file(&path, 2).unwrap();
        store
            .edit([((0, 0), "x".to_string()), ((3, 1), "9".to_string())])
            .unwrap();

        // Only the edit on the current page is reported, positioned within it.
        assert_eq!(store.edited_cells(), vec![(0, 0)]);

        store.scroll_to_offset(2).unwrap();
        assert_eq!(store.edited_cells(), vec![(1, 1)]);
    }

    #[test]
    fn undo_and_redo_move_the_page_with_them() {
        let path = write_temp("undo.csv", SAMPLE);
        let mut store = Store::open_file(&path, 10).unwrap();
        store.edit([((0, 0), "x".to_string())]).unwrap();

        assert!(store.undo().unwrap());
        assert_eq!(cell(&store.current_view, 0, 0).as_deref(), Some("a"));
        assert_eq!(store.dirty(), 0);

        assert!(store.redo().unwrap());
        assert_eq!(cell(&store.current_view, 0, 0).as_deref(), Some("x"));
        assert!(!store.redo().unwrap());
    }

    #[test]
    fn a_sorted_view_refuses_edits() {
        let path = write_temp("sorted.csv", SAMPLE);
        let mut store = Store::open_file(&path, 10).unwrap();
        assert_eq!(store.edit_blocked(), None);

        let _rx = store.begin_sort(0);
        let blocked = store.edit_blocked().expect("sorted views are not editable");
        assert!(blocked.contains("sorted"), "{blocked}");

        store.clear_sort().unwrap();
        assert_eq!(store.edit_blocked(), None);
    }

    #[test]
    fn a_store_without_a_file_behind_it_is_read_only() {
        let path = write_temp("readonly.csv", SAMPLE);
        let store = Store::new(loader::load(&path).unwrap(), 10).unwrap();
        assert!(store.edit_blocked().is_some());
    }

    #[test]
    fn saving_writes_the_file_and_empties_the_buffer() {
        let path = write_temp("save.csv", SAMPLE);
        let mut store = Store::open_file(&path, 10).unwrap();
        store.edit([((1, 1), "99".to_string())]).unwrap();
        store.save(None, false).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "name,count\na,1\nb,99\nc,3\nd,4\n"
        );
        assert_eq!(store.dirty(), 0);
        // The reopened file shows the written value, not a stale overlay.
        assert_eq!(cell(&store.current_view, 1, 1).as_deref(), Some("99"));
    }

    #[test]
    fn a_second_save_is_not_refused_by_our_own_first_one() {
        let path = write_temp("twice.csv", SAMPLE);
        let mut store = Store::open_file(&path, 10).unwrap();
        store.edit([((0, 0), "x".to_string())]).unwrap();
        store.save(None, false).unwrap();

        store.edit([((1, 0), "y".to_string())]).unwrap();
        store
            .save(None, false)
            .expect("the stamp must follow the write");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "name,count\nx,1\ny,2\nc,3\nd,4\n"
        );
    }

    #[test]
    fn a_file_changed_underneath_the_buffer_is_not_overwritten() {
        let path = write_temp("stale.csv", SAMPLE);
        let mut store = Store::open_file(&path, 10).unwrap();
        store.edit([((0, 0), "x".to_string())]).unwrap();

        std::fs::write(&path, "name,count\nz,9\n").unwrap();
        let err = store.save(None, false).unwrap_err().to_string();
        assert!(err.contains("changed on disk"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "name,count\nz,9\n");
        assert_eq!(store.dirty(), 1, "the edit is still pending");
    }

    #[test]
    fn writing_elsewhere_leaves_the_buffer_dirty() {
        let path = write_temp("source.csv", SAMPLE);
        let other = write_temp("copy.csv", "placeholder\n");
        let mut store = Store::open_file(&path, 10).unwrap();
        store.edit([((0, 0), "x".to_string())]).unwrap();
        store.save(Some(&other), false).unwrap();

        assert_eq!(
            std::fs::read_to_string(&other).unwrap(),
            "name,count\nx,1\nb,2\nc,3\nd,4\n"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SAMPLE);
        assert_eq!(store.dirty(), 1, ":w path does not save the source file");
    }
}
