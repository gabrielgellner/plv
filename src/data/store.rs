use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::{self, yield_now};

use anyhow::{Result, bail};
use duckdb::Connection;
use polars::prelude::*;

use crate::data::budget;
use crate::data::edit::{Cell, Overlay};
use crate::data::index::{self, RowIndex};
use crate::data::lake_db::{self, LakeSource};
use crate::data::loader;
use crate::data::rows::{self, RowSet};
use crate::data::writer::{self, Stamp};
use crate::view::View;

/// The column a materialised sort carries to remember where each row came
/// from in the file. Added before the sort, so it records the file's order.
const SOURCE_ROW: &str = "__src__";

/// A sorted frame is kept in memory, so there has to be a point past which it
/// is not — and past which plv declines to sort at all. The size of that point
/// comes from [`budget`], which asks the machine rather than assuming one.
///
/// Falling back to a lazy sort was the obvious kindness and is the wrong one.
/// Polars has to read and rank every row either way, so a lazy sort of a table
/// that does not fit does not degrade, it just fails slowly: measured against
/// an 842M-row census parquet, the lazy path reached **12.5GB resident in 45
/// seconds** without producing a page. Refusing is the honest answer.
///
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
    /// Where each row of a delimited file starts, so a page can be read
    /// without parsing everything before it. Absent for Parquet, which can
    /// already seek, and for lake tables.
    row_index: Option<std::sync::Arc<RowIndex>>,
    /// Overrides how many bytes a scan chunk may read.
    ///
    /// Only tests set it. Crossing a chunk seam otherwise needs a fixture of
    /// tens of megabytes, and the row numbering across that seam is precisely
    /// where an off-by-one would hide.
    scan_bytes: Option<u64>,
    /// The whole sorted table, held in memory, carrying [`SOURCE_ROW`].
    ///
    /// Sorting cannot be lazy — nothing can know which row comes first
    /// without reading them all — so a lazily sorted page costs a full read
    /// of the file, *every page*. Doing it once and keeping the answer costs
    /// the same as one of those pages and makes the rest free. It is also
    /// what gives a sorted view row identity, which is what lets it be
    /// edited.
    sorted: Option<DataFrame>,
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
            row_index: None,
            scan_bytes: None,
            sorted: None,
        })
    }

    /// Open a store over a file, remembering the path so edits can be written
    /// back to it.
    pub fn open_file(path: &Path, viewport_rows: usize) -> Result<Self> {
        // Only delimited text is editable. Parquet is genuinely typed, so a
        // one-cell change would mean rewriting the whole file against a schema.
        let separator = loader::separator(path)?;

        // A delimited file has to be read through once to know how many rows
        // it has. That same pass records where the rows are, so paging into it
        // later does not have to count its way there.
        let row_index = match separator {
            Some(separator) => Some(std::sync::Arc::new(RowIndex::build(path, separator)?)),
            None => None,
        };
        let rows = row_index.as_ref().map(|index| index.rows());

        let mut store = Self::build(loader::load(path)?, viewport_rows, rows)?;
        store.row_index = row_index;
        if let Some(separator) = separator {
            store.edit = Some(EditTarget {
                path: path.to_path_buf(),
                separator,
                stamp: Stamp::of(path)?,
            });
        }
        store.current_view = store.fetch(0, viewport_rows)?;
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
            row_index: None,
            scan_bytes: None,
            sorted: None,
        })
    }

    // ── rows: a filter replaces the row space ────────────────────────────

    /// Rows on show. Not `total_rows`, which stays the file's own count —
    /// the writer needs that to check it is looking at the same file.
    pub fn row_count(&self) -> usize {
        match (&self.sorted, &self.filter_rows) {
            (Some(sorted), _) => sorted.height(),
            (None, Some(set)) => set.len(),
            // Deleted rows are struck rather than removed: still in the
            // file, simply not counted among what is on show.
            (None, None) => self.total_rows.saturating_sub(self.overlay.struck_count()),
        }
    }

    /// The source row behind a display position.
    pub fn source_row(&self, display: usize) -> Option<usize> {
        if let Some(sorted) = &self.sorted {
            // The sort carried the file's row numbers along with it.
            return sorted
                .column(SOURCE_ROW)
                .ok()?
                .u32()
                .ok()?
                .get(display)
                .map(|row| row as usize);
        }
        match &self.filter_rows {
            Some(set) => set.source(display),
            None => {
                let row = self.skip_struck(display);
                (row < self.total_rows).then_some(row)
            }
        }
    }

    /// The source row at a display position, stepping over the struck ones.
    ///
    /// Walks the struck set, which is ascending, so it costs the number of
    /// deletions before the row rather than a search of the file. Deleting a
    /// handful of rows is what this is for; deleting a great many would make
    /// a running count worth keeping instead.
    fn skip_struck(&self, display: usize) -> usize {
        let mut row = display;
        for struck in self.overlay.struck() {
            if struck <= row {
                row += 1;
            } else {
                break;
            }
        }
        row
    }

    /// Turn a row index reported by a background search into a display
    /// position.
    ///
    /// The search reads whatever frame the viewer is reading, so under a sort
    /// it already reports display positions. Under a filter it reads the file
    /// and reports the file's rows, which have to be looked up — and a row the
    /// filter dropped has nowhere to go.
    pub fn search_row_to_display(&self, reported: usize) -> Option<usize> {
        if self.sorted.is_some() {
            return Some(reported);
        }
        match &self.filter_rows {
            Some(set) => set.display(reported),
            None => (reported < self.total_rows).then_some(reported),
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
        if let Some(sorted) = &self.sorted {
            let df = self.page_of_sorted(sorted, offset, height)?;
            return self.apply_overlay(df, offset);
        }
        let df = match &self.source {
            Source::Lazy(_) => {
                // Which rows of the file this page shows: picked out by a
                // filter, missing the ones deleted, or simply the next few in
                // order when neither applies.
                let wanted: Vec<usize> = (0..height)
                    .filter_map(|i| self.source_row(offset + i))
                    .collect();
                self.rows_at(&wanted)
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

    /// The named source rows, read by whatever means is cheapest.
    ///
    /// Every page of a delimited file comes through here — rows a filter
    /// picked out, rows left after deletions, or simply the next few in order.
    /// The span between the first and last is read once and the wanted rows
    /// taken from it. The span is the unavoidable part, since those rows have
    /// to be read, and there is nothing to pick out at all when nothing in
    /// between was left out.
    fn rows_at(&self, wanted: &[usize]) -> Result<DataFrame> {
        let (Some(&first), Some(&last)) = (wanted.first(), wanted.last()) else {
            // No rows, but the caller still needs the right columns.
            let lf = self.effective_lf().expect("lazy source");
            return Ok(lf.slice(0, 0).collect()?);
        };
        let span = self.fetch_span(first, last - first + 1)?;
        if wanted.len() == last - first + 1 {
            return Ok(span);
        }
        let picked: Vec<IdxSize> = wanted.iter().map(|&row| (row - first) as IdxSize).collect();
        Ok(span.take(&IdxCa::from_vec(PlSmallStr::from_static("i"), picked))?)
    }

    /// Rows `first..first + span` of the source.
    ///
    /// Read from the byte offset the index points at when there is one and
    /// nothing is composed on top — a sort or a projection changes what a row
    /// number means, so those go the lazy way.
    fn fetch_span(&self, first: usize, span: usize) -> Result<DataFrame> {
        if self.view.sort.is_empty()
            && self.view.select.is_none()
            && let Some(page) = self.indexed_span(first, span)
        {
            return page;
        }
        let lf = self.effective_lf().expect("lazy source");
        Self::fetch_lazy(&lf, first, span)
    }

    /// A page of the materialised sorted frame: sliced, with the bookkeeping
    /// column dropped and the view's projection applied.
    fn page_of_sorted(
        &self,
        sorted: &DataFrame,
        offset: usize,
        height: usize,
    ) -> Result<DataFrame> {
        let page = sorted.slice(offset as i64, height);
        let shown: Vec<PlSmallStr> = self
            .columns()
            .into_iter()
            .filter_map(|source| self.schema.get_at_index(source))
            .map(|(name, _)| name.clone())
            .collect();
        Ok(page.select(shown)?)
    }

    /// A span of rows, read straight out of the byte range the index points at.
    ///
    /// `None` when there is no index to ask, leaving the caller to slice the
    /// frame the slow way. The parse is given the schema plv already inferred
    /// — a chunk left to infer its own would type a column by whatever
    /// happens to be in those rows, and the types would change as you scroll.
    fn indexed_span(&self, offset: usize, height: usize) -> Option<Result<DataFrame>> {
        let index = self.row_index.as_ref()?;
        let target = self.edit.as_ref()?;
        if offset >= index.rows() {
            return None;
        }
        let (first_row, from) = index.seek(offset);
        let to = index.end_of(offset + height);

        Some((|| {
            let bytes = index::read_span(&target.path, index, from, to)?;
            let page = parse_span(bytes, &self.schema, target.separator)?;
            // The span starts at a checkpoint, which is at or before the page.
            Ok(page.slice((offset - first_row) as i64, height))
        })())
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
        drop(tx);
        drop(rx);
        self.resort()
    }

    /// Rebuild whatever the current sort needs, off the main thread.
    ///
    /// For a frame that fits, that is the whole sorted table: paying one full
    /// read once instead of one per page. Past the cap, and for lake tables,
    /// it stays the old first-page fetch — the sort is redone per page, which
    /// is slow but works.
    pub fn resort(&mut self) -> mpsc::Receiver<DataFrame> {
        let (tx, rx) = mpsc::channel();
        self.sorted = None;
        // A held frame carries its own filter, so the row set has nothing left
        // to save: the table it would spare us re-reading is already in memory.
        self.filter_rows = None;
        self.row_offset = 0;

        if self.view.sort.is_empty() {
            let _ = self.refresh();
            return rx;
        }

        let vp = self.viewport_rows;
        let keys = self.sort_keys();

        match &self.source {
            // Held or not at all: see `budget::sort_cells`. Callers ask
            // `sort_blocked` first, so reaching here past the cap would be a
            // bug rather than a slow path.
            Source::Lazy(base) => {
                let base = base.clone();
                let filter = self.view.filter.clone();
                let schema = self.schema.clone();
                thread::spawn(move || {
                    // Built on the worker: an `Expr` is not `Send`.
                    let predicate = filter.and_then(|f| rows::predicate(&f, &schema));
                    if let Ok(df) = Self::materialise(base, &keys, predicate) {
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
                thread::spawn(move || {
                    if let Ok(df) = lake_db::page_with(&conn, &source, &keys, 0, vp) {
                        let _ = tx.send(df);
                    }
                });
            }
        }
        rx
    }

    /// Whether the sorted frame is small enough to keep.
    ///
    /// A filter that has finished resolving has already narrowed the table, so
    /// the question is how many rows are on show rather than how many the file
    /// holds — which is what lets a sort be applied to a small slice of a table
    /// far too big to sort whole.
    fn sort_fits(&self) -> bool {
        let rows = match &self.filter_rows {
            Some(set) if set.is_complete() => set.len(),
            _ => self.total_rows,
        };
        sort_fits(rows, self.schema.len())
    }

    /// Why this store will not sort, or `None` when it will.
    ///
    /// Asked *before* a sort key is recorded, so a refusal leaves the view as
    /// it was rather than in an order nothing can produce.
    pub fn sort_blocked(&self) -> Option<String> {
        if matches!(self.source, Source::Lazy(_)) && !self.sort_fits() {
            return Some(format!(
                "{} rows across {} columns is more than there is memory to sort",
                self.row_count(),
                self.schema.len()
            ));
        }
        None
    }

    /// Sort the whole frame, carrying the file's row numbers through it.
    ///
    /// The row index goes on *before* the sort, so it records where each row
    /// came from rather than where it ended up.
    /// The row index goes on *before* the filter, so it records where each
    /// row came from rather than where it survived to; the filter goes on
    /// before the sort, matching the order the view language documents.
    fn materialise(
        base: LazyFrame,
        keys: &[(String, bool)],
        predicate: Option<Expr>,
    ) -> Result<DataFrame> {
        let (names, descending): (Vec<String>, Vec<bool>) = keys
            .iter()
            .cloned()
            .map(|(name, ascending)| (name, !ascending))
            .unzip();
        let mut lf = base.with_row_index(SOURCE_ROW, None);
        if let Some(predicate) = predicate {
            lf = lf.filter(predicate);
        }
        Ok(lf
            .sort(
                names,
                SortMultipleOptions::default().with_order_descending_multi(descending),
            )
            .collect()?)
    }

    /// Take the result of a sort.
    ///
    /// A frame carrying [`SOURCE_ROW`] is the whole sorted table and is kept;
    /// anything else is one page, fetched the old way because the table was
    /// too big to hold.
    pub fn adopt_sorted(&mut self, df: DataFrame) -> Result<()> {
        if df.column(SOURCE_ROW).is_ok() {
            self.sorted = Some(df);
            self.refresh()
        } else {
            self.current_view = self.apply_overlay(df, self.row_offset)?;
            Ok(())
        }
    }

    /// Clear all sort keys and return to natural order.
    pub fn clear_sort(&mut self) -> Result<()> {
        self.view.sort.clear();
        self.sorted = None;
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
        if !self.view.sort.is_empty() && self.sorted.is_none() {
            // Without the sorted frame there is no record of where each row
            // came from, so an edit could not be told which line it belongs
            // to. Which of the two reasons it is matters: one passes.
            // Sorting past the cap is refused outright, so a sort that is
            // set but not held can only be one still being built.
            return Some("still sorting");
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

    /// Everything pending: cells edited and rows deleted.
    pub fn dirty(&self) -> usize {
        self.overlay.pending()
    }

    /// Why rows cannot be deleted here, or `None` when they can.
    ///
    /// A sort or a filter puts an explicit list of rows on screen, and taking
    /// one out of the middle would mean rebuilding that list — a different
    /// piece of work from striking a row out of the file, and one worth doing
    /// deliberately.
    pub fn delete_blocked(&self) -> Option<&'static str> {
        if let Some(reason) = self.edit_blocked() {
            return Some(reason);
        }
        if self.filter_rows.is_some() || self.sorted.is_some() {
            return Some("cannot delete rows from a filtered or sorted view");
        }
        None
    }

    /// Strike out the rows at these display positions, as one undoable step.
    pub fn delete_rows<I: IntoIterator<Item = usize>>(&mut self, rows: I) -> Result<usize> {
        let struck: Vec<usize> = rows
            .into_iter()
            .filter_map(|d| self.source_row(d))
            .collect();
        let count = struck.len();
        self.overlay.strike(struck);
        self.refresh()?;
        Ok(count)
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
        // The held sort describes the file as it was before the write.
        self.sorted = None;
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
        let mut values: Vec<Option<String>> =
            text.str()?.iter().map(|v| v.map(str::to_string)).collect();
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
        // Asked row by row rather than edit by edit. A filter or a sort can
        // put any file row at any display position, so going the other way
        // would mean searching for each edit; going this way is one lookup per
        // row on screen, whatever the view is doing.
        (0..height).filter_map(move |local| {
            let source = self.source_row(offset + local)?;
            Some((local, self.overlay.row(source)?))
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
        // A held sort is both the frame on screen and much the faster thing to
        // scan; without one, fall back to the lazy pipeline.
        let (lf, total) = match &self.sorted {
            Some(sorted) => (sorted.clone().lazy(), sorted.height()),
            None => match self.effective_lf() {
                Some(lf) => (lf, self.total_rows),
                None => return,
            },
        };
        let schema = self.schema.clone();
        // Built inside the thread: an `Expr` is not `Send`.
        let build = {
            let schema = schema.clone();
            let pattern = pattern.clone();
            let col_name = col_name.clone();
            move || match col_name {
                Some(name) => Some(matches_pattern(&name, &pattern)),
                None => schema
                    .iter_names()
                    .map(|name| matches_pattern(name.as_str(), &pattern))
                    .reduce(Expr::or),
            }
        };

        // The indexed scan reads the file itself, so it can only stand in for
        // the lazy one when the view has not changed what a row is.
        let plain =
            self.sorted.is_none() && self.view.sort.is_empty() && self.view.select.is_none();
        if plain && self.scan_indexed(tx.clone(), build).is_some() {
            return;
        }
        Self::scan_rows(lf, total, tx, move || match col_name {
            Some(name) => Some(matches_pattern(&name, &pattern)),
            None => schema
                .iter_names()
                .map(|name| matches_pattern(name.as_str(), &pattern))
                .reduce(Expr::or),
        });
    }

    /// Scan a delimited file chunk by chunk, reading each chunk from the byte
    /// offset the index points at.
    ///
    /// The lazy alternative re-reads from the top of the file for every chunk,
    /// so its cost grows with the offset and the whole scan is quadratic:
    /// measured on a 437MB CSV, a 10,000-row chunk costs 10ms at the start and
    /// 494ms twenty million rows in. Reading each chunk where it actually
    /// lives makes the scan linear.
    ///
    /// `None` when there is no index to read from, leaving the caller on the
    /// lazy path.
    #[allow(clippy::too_many_arguments)]
    fn scan_indexed<F>(&self, tx: mpsc::Sender<Vec<usize>>, build: F) -> Option<()>
    where
        F: FnOnce() -> Option<Expr> + Send + 'static,
    {
        let index = self.row_index.clone()?;
        let target = self.edit.as_ref()?;
        let path = target.path.clone();
        let separator = target.separator;
        let schema = self.schema.clone();
        let budget = self.scan_bytes.unwrap_or_else(budget::scan_bytes);

        thread::spawn(move || {
            let Some(predicate) = build() else { return };
            let total = index.rows();
            let mut start = 0usize;

            while start < total {
                // Sized by what it will read, not by a row count: a chunk of
                // n rows is a few megabytes in one file and gigabytes in
                // another, and only the bytes bound the memory.
                let end = index.chunk_end(start, budget);
                let (_, from) = index.seek(start);
                let to = index.end_of(end);

                let Ok(bytes) = index::read_span(&path, &index, from, to) else {
                    break;
                };
                let Ok(chunk) = parse_span(bytes, &schema, separator) else {
                    break;
                };
                let Ok(hits) = chunk
                    .lazy()
                    .with_row_index(MATCH_ROW, Some(start as u32))
                    .filter(predicate.clone())
                    .select([col(MATCH_ROW)])
                    .collect()
                else {
                    break;
                };

                // Sent even when empty, so a scan that is merely finding
                // nothing cannot be mistaken for one that has stalled.
                if tx.send(match_rows(&hits)).is_err() {
                    return; // receiver dropped — cancelled
                }
                start = end;
            }
        });
        Some(())
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
                    .with_row_index(MATCH_ROW, Some(offset as u32))
                    .filter(predicate.clone())
                    .select([col(MATCH_ROW)])
                    .collect()
                else {
                    break;
                };

                if tx.send(match_rows(&df)).is_err() {
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
        // This is the no-sort path, so any held frame is in an order the view
        // no longer asks for.
        self.sorted = None;
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

        self.filter_rows = Some(RowSet::new(budget::filter_rows()));
        self.row_offset = 0;
        self.refresh()?;

        let predicate = {
            let filter = filter.clone();
            let schema = schema.clone();
            move || rows::predicate(&filter, &schema)
        };
        // The index makes the scan linear; without one it re-reads from the
        // top of the file for every chunk.
        if self.scan_indexed(tx.clone(), predicate).is_none() {
            Self::scan_rows(lf, self.total_rows, tx, move || {
                rows::predicate(&filter, &schema)
            });
        }
        Ok(Some(rx))
    }

    /// Take a batch of matching rows from the scan.
    /// Take a batch of matching rows from the scan.
    ///
    /// Returns whether the scan is still wanted: once the set is as large as
    /// the budget allows, it keeps what it has and the caller drops the
    /// receiver, which stops the thread.
    pub fn extend_filter(&mut self, batch: Vec<usize>) -> Result<bool> {
        let wanted = match &mut self.filter_rows {
            Some(set) => set.extend(batch),
            None => false,
        };
        self.refresh()?;
        Ok(wanted)
    }

    /// Whether the filter stopped short of every match.
    pub fn filter_truncated(&self) -> bool {
        self.filter_rows.as_ref().is_some_and(RowSet::is_truncated)
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

/// The column a scan puts its row numbers in.
const MATCH_ROW: &str = "__idx__";

/// The row numbers a scan's chunk turned up.
fn match_rows(hits: &DataFrame) -> Vec<usize> {
    hits.column(MATCH_ROW)
        .ok()
        .and_then(|c| c.u32().ok())
        .map(|ca| ca.iter().flatten().map(|i| i as usize).collect())
        .unwrap_or_default()
}

/// Parse a span of a delimited file that was read with its header in front.
///
/// Given the schema plv already inferred, never left to infer its own: a chunk
/// would type each column by whatever happens to be in those rows, so the same
/// column could come back differently from two different chunks.
fn parse_span(bytes: Vec<u8>, schema: &SchemaRef, separator: u8) -> Result<DataFrame> {
    Ok(CsvReadOptions::default()
        .with_has_header(true)
        .with_schema(Some(schema.clone()))
        .with_parse_options(CsvParseOptions::default().with_separator(separator))
        .into_reader_with_file_handle(std::io::Cursor::new(bytes))
        .finish()?)
}

/// Whether a table of this shape can be sorted at all: whether holding it
/// would fit the memory budget.
fn sort_fits(rows: usize, columns: usize) -> bool {
    rows.saturating_mul(columns) <= budget::sort_cells()
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

    /// Small fixtures never reach a second checkpoint, so the interesting
    /// case — a page found by seeking rather than by counting — needs a file
    /// bigger than one stride.
    /// The indexed scan reads spans of the file itself, so it has to find
    /// exactly what a filter over the whole frame would.
    #[test]
    fn an_indexed_scan_finds_the_same_rows_across_several_chunks() {
        use crate::data::index::STRIDE;

        // Wide enough that the fixture runs past one chunk's byte budget, or
        // the seams between chunks never get crossed.
        let rows = STRIDE * 3 + 500;
        let padding = "x".repeat(48);
        let mut csv = String::from("id,cat,filler\n");
        for i in 0..rows {
            csv.push_str(&format!(
                "{i},{},{padding}\n",
                if i % 3 == 0 { "a" } else { "b" }
            ));
        }
        let path = write_temp("scan.csv", &csv);
        let mut store = Store::open_file(&path, 10).unwrap();
        // Force several chunks out of a small fixture, so the row numbering
        // across the seams is what is being checked.
        store.scan_bytes = Some(64 << 10);
        let index = store.row_index.clone().expect("a csv has an index");
        assert!(
            index.chunk_end(0, 64 << 10) < rows,
            "the budget was meant to force more than one chunk"
        );

        store.view.filter = Some(
            match crate::view::parse("filter cat = a", &store.schema).unwrap() {
                crate::view::Command::Filter(filter) => filter,
                other => panic!("{other:?}"),
            },
        );
        let rx = store.begin_filter().unwrap().unwrap();
        while let Ok(batch) = rx.recv_timeout(std::time::Duration::from_secs(30)) {
            if !store.extend_filter(batch).unwrap() {
                break;
            }
        }
        store.finish_filter().unwrap();

        let expected = rows.div_ceil(3);
        assert_eq!(store.row_count(), expected, "every third row matches");
        // And they are the right rows, in file order, across chunk seams.
        assert_eq!(store.source_row(0), Some(0));
        assert_eq!(store.source_row(1), Some(3));
        let last = store.row_count() - 1;
        assert_eq!(store.source_row(last), Some((expected - 1) * 3));
    }

    #[test]
    fn an_indexed_page_reads_the_same_rows_the_slow_path_would() {
        use crate::data::index::STRIDE;

        let rows = STRIDE * 2 + 1_000;
        let mut csv = String::from("id,note\n");
        for i in 0..rows {
            // A quoted comma every so often, so the byte offsets cannot be
            // arrived at by assuming fixed-width records.
            if i % 7 == 0 {
                csv.push_str(&format!("{i},\"a,b\"\n"));
            } else {
                csv.push_str(&format!("{i},plain\n"));
            }
        }
        let path = write_temp("indexed.csv", &csv);
        let mut store = Store::open_file(&path, 10).unwrap();
        assert_eq!(store.total_rows, rows);

        // Either side of both checkpoints, and the last page.
        for offset in [
            0,
            5,
            STRIDE - 3,
            STRIDE,
            STRIDE + 4,
            2 * STRIDE + 900,
            rows - 10,
        ] {
            store.scroll_to_offset(offset).unwrap();
            let landed = store.row_offset;
            let first = cell(&store.current_view, 0, 0).unwrap();
            assert_eq!(
                first,
                landed.to_string(),
                "page at {offset} began on the wrong row"
            );
            let note = cell(&store.current_view, 1, 0).unwrap();
            let expected = if landed % 7 == 0 { "a,b" } else { "plain" };
            assert_eq!(note, expected, "row {landed} came back with the wrong note");
        }
    }

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

    /// Sorting runs off the main thread; wait for it and take the result.
    fn settle_sort(store: &mut Store, rx: std::sync::mpsc::Receiver<DataFrame>) {
        let df = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the sort never finished");
        store.adopt_sorted(df).unwrap();
    }

    #[test]
    fn a_table_too_big_to_hold_is_not_sorted_at_all() {
        // Measured against an 842M-row census parquet: sorting it lazily
        // reached 12.5GB resident in 45s without producing a page, so the
        // answer past the cap is no rather than "slowly".
        let cap = budget::sort_cells();
        assert!(sort_fits(cap, 1));
        assert!(!sort_fits(cap + 1, 1));
        assert!(!sort_fits(842_209_475, 20), "the census parquet");
        // Multiplying the shape must not wrap into a false yes.
        assert!(!sort_fits(usize::MAX, 20));
    }

    #[test]
    fn an_ordinary_file_is_sortable() {
        let path = write_temp("sortable.csv", SAMPLE);
        let store = Store::open_file(&path, 10).unwrap();
        assert_eq!(store.sort_blocked(), None);
    }

    #[test]
    fn a_held_sort_keeps_the_rows_identifiable_and_editable() {
        let path = write_temp("sorted.csv", SAMPLE);
        let mut store = Store::open_file(&path, 10).unwrap();
        assert_eq!(store.edit_blocked(), None);

        let rx = store.begin_sort(0);
        // Until it lands there is no record of where the rows came from.
        assert_eq!(store.edit_blocked(), Some("still sorting"));
        settle_sort(&mut store, rx);

        // Held, so every displayed row knows its line in the file — which is
        // what editing a sorted view needs.
        assert_eq!(store.edit_blocked(), None);
        assert_eq!(store.row_count(), 4);
        for display in 0..4 {
            assert!(store.source_row(display).is_some(), "row {display}");
        }

        store.clear_sort().unwrap();
        assert_eq!(store.edit_blocked(), None);
    }

    #[test]
    fn a_descending_sort_reverses_the_page_and_remembers_the_file_order() {
        let path = write_temp("sortorder.csv", SAMPLE);
        let mut store = Store::open_file(&path, 10).unwrap();

        let rx = store.begin_sort(0); // ascending by name
        settle_sort(&mut store, rx);
        assert_eq!(cell(&store.current_view, 0, 0).as_deref(), Some("a"));
        assert_eq!(store.source_row(0), Some(0));

        let rx = store.begin_sort(0); // pressing again reverses it
        settle_sort(&mut store, rx);
        assert_eq!(cell(&store.current_view, 0, 0).as_deref(), Some("d"));
        assert_eq!(store.source_row(0), Some(3), "d is the file's fourth row");

        // The bookkeeping column is not something the viewer shows.
        assert_eq!(store.current_view.width(), 2);
    }

    #[test]
    fn an_edit_through_a_sorted_view_lands_on_the_right_line() {
        let path = write_temp("sortedit.csv", SAMPLE);
        let mut store = Store::open_file(&path, 10).unwrap();
        let rx = store.begin_sort(0);
        settle_sort(&mut store, rx);
        let rx = store.begin_sort(0); // descending: d, c, b, a
        settle_sort(&mut store, rx);

        // Display row 0 is `d`, the file's fourth row.
        store.edit([((0, 1), "99".to_string())]).unwrap();
        assert_eq!(cell(&store.current_view, 1, 0).as_deref(), Some("99"));

        store.save(None, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "name,count\na,1\nb,2\nc,3\nd,99\n"
        );
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
