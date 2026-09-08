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
use crate::data::jsonl;
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
    /// A JSONL file, parsed by plv rather than by Polars — see
    /// [`crate::data::jsonl`]. There is no `LazyFrame` behind it: every page
    /// is built from the byte span the row index points at, which is the
    /// cheap path anyway, and the only one that keeps a nested value as the
    /// document it was.
    Json(JsonFile),
}

/// A JSONL file and where each of its columns lives in a record.
///
/// The paths are in the schema's own order, one per column. A column found
/// when the file opened is one key deep; `:expand` adds longer ones.
struct JsonFile {
    path: PathBuf,
    columns: Vec<jsonl::KeyPath>,
}

struct LakeQuery {
    conn: Connection,
    source: LakeSource,
    columns: Vec<String>,
}

/// How a file plv writes to is spelled: a delimited record, or a JSON one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Written {
    Delimited(u8),
    Json,
}

/// A file plv can write edits back to.
struct EditTarget {
    path: PathBuf,
    written: Written,
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
    /// What opening the file turned up that the user should be told once:
    /// keys left out, lines that were not records.
    notes: Option<String>,
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
            notes: None,
        })
    }

    /// Open a store over a file, remembering the path so edits can be written
    /// back to it.
    pub fn open_file(path: &Path, viewport_rows: usize) -> Result<Self> {
        if matches!(loader::detect_format(path), loader::FileFormat::JsonLines) {
            return Self::open_jsonl(path, viewport_rows);
        }
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
                written: Written::Delimited(separator),
                stamp: Stamp::of(path)?,
            });
        }
        store.current_view = store.fetch(0, viewport_rows)?;
        Ok(store)
    }

    /// Open a store over a JSONL file.
    ///
    /// One pass does everything: the row index and the key discovery read the
    /// file together, so the column set is the file's true one rather than a
    /// sample of it, and it is settled before the first frame is drawn —
    /// unlike a reader that adds columns as it meets them and moves the table
    /// sideways while you are reading.
    fn open_jsonl(path: &Path, viewport_rows: usize) -> Result<Self> {
        let mut scan = jsonl::Scan::new();
        let index = RowIndex::build_lines(path, &mut |byte| scan.byte(byte))?;
        let fields = scan.finish();

        let schema = fields.schema();
        if schema.is_empty() {
            anyhow::bail!("no JSON objects in {}", path.display());
        }
        let view = View {
            // Rare keys are reachable through `C` and `:select`; a table that
            // opens two hundred columns wide has answered no question.
            select: fields.shown(),
            ..View::default()
        };

        let mut store = Self {
            source: Source::Json(JsonFile {
                path: path.to_path_buf(),
                columns: fields.paths(),
            }),
            schema,
            total_rows: index.rows(),
            row_offset: 0,
            viewport_rows,
            current_view: DataFrame::empty(),
            view,
            edit: Some(EditTarget {
                path: path.to_path_buf(),
                written: Written::Json,
                stamp: Stamp::of(path)?,
            }),
            overlay: Overlay::new(),
            filter_rows: None,
            row_index: Some(std::sync::Arc::new(index)),
            scan_bytes: None,
            sorted: None,
            notes: None,
        };
        store.notes = fields.notes();
        store.current_view = store.fetch(0, viewport_rows)?;
        Ok(store)
    }

    /// Lift the documents in one column out into columns of their own.
    ///
    /// The other direction from `K`, which reads one document whole: this is
    /// for when the same shape is in every record and the interesting part is
    /// comparing one field of it down the file. VisiData's `(`, and the reason
    /// it has one.
    ///
    /// **Additive.** The new columns go on the end of the schema and the
    /// parent's place in the view is taken by its children — the parent is
    /// still there, one `:reset select` or `C` away, and so is `K` on it.
    /// Appending rather than inserting is what keeps every source column index
    /// meaning what it did: the widths, the pins and the edit overlay are all
    /// keyed by those numbers, and renumbering them under a display command
    /// would quietly move somebody's pin. For the same reason there is no
    /// un-expand: hiding the columns is `-`, and taking them out of the schema
    /// would renumber everything after them.
    pub fn expand(&mut self, source_col: usize) -> Result<String> {
        let Source::Json(file) = &self.source else {
            bail!("only jsonl columns can be expanded");
        };
        let Some(path) = file.columns.get(source_col).cloned() else {
            bail!("no such column");
        };
        let name = match self.schema.get_at_index(source_col) {
            Some((name, _)) => name.to_string(),
            None => bail!("no such column"),
        };

        let sample = jsonl::sample_under(&file.path, &path)?;
        let children = sample.fields.columns();
        if children.is_empty() {
            bail!("nothing to expand: no documents in {name}");
        }
        if children
            .iter()
            .all(|child| self.schema.contains(&format!("{name}.{}", child.name)[..]))
        {
            bail!("{name} is already expanded");
        }

        // Read before the schema grows, since it is the old numbering that
        // the view is written in.
        let mut shown = self.view.columns(self.schema.len());
        let mut schema = (*self.schema).clone();
        let mut paths = file.columns.clone();
        let mut added = Vec::new();
        for child in children {
            let name = unique_name(&schema, format!("{name}.{}", child.name));
            added.push(schema.len());
            schema.with_column(name.into(), child.dtype());
            paths.push([path.clone(), vec![child.name.clone()]].concat());
        }

        let count = added.len();
        match shown.iter().position(|col| *col == source_col) {
            // The children stand where their parent stood, so the table does
            // not shuffle sideways around the column being read.
            Some(at) => {
                shown.splice(at..=at, added);
            }
            // Expanding a column that is not on show puts them at the end,
            // which is where a column with no place of its own goes.
            None => shown.extend(added),
        }

        self.schema = Arc::new(schema);
        if let Source::Json(file) = &mut self.source {
            file.columns = paths;
        }
        self.view.select = Some(shown);
        // The held frame was built against the old schema and has no such
        // columns; the caller re-sorts.
        self.sorted = None;
        self.refresh()?;

        Ok(match sample.complete {
            true => format!("{name} expanded into {count} columns"),
            // What was read is what was in reach, and a key further down the
            // file would not be here. Said out loud, as `(first n)` is.
            false => format!(
                "{name} expanded into {count} columns (from the first {} records with it)",
                jsonl::SAMPLE_RECORDS
            ),
        })
    }

    /// What the store wants said about the file when it opened, if anything.
    pub fn notes(&self) -> Option<&str> {
        self.notes.as_deref()
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
            notes: None,
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
            // file, simply not counted among what is on show. Added ones are
            // the other way round — counted, but not in the file yet.
            (None, None) => (self.total_rows + self.overlay.added_count())
                .saturating_sub(self.overlay.struck_count()),
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
            None => self.walk_to(display),
        }
    }

    /// Whether a row number is one of the file's, or an added row's id.
    ///
    /// Added rows are numbered past the end of the file, so the two can never
    /// be confused and the same `(row, column)` key works for both.
    pub fn is_added(&self, row: usize) -> bool {
        row >= self.total_rows
    }

    /// What sits at a display position: a row of the file, stepping over the
    /// struck ones, or the id of a row added before it.
    ///
    /// Walks the added anchors and the struck set, both ascending and both
    /// expected to be small — this is for adding and removing a handful of
    /// rows, not for rewriting the file.
    fn walk_to(&self, display: usize) -> Option<usize> {
        let mut remaining = display;
        let mut from = 0usize;

        for (anchor, ids) in self.overlay.added() {
            let surviving = (anchor - from) - self.overlay.struck_in(from..anchor);
            if remaining < surviving {
                return Some(self.nth_surviving(from, remaining));
            }
            remaining -= surviving;
            from = anchor;

            if remaining < ids.len() {
                return Some(ids[remaining]);
            }
            remaining -= ids.len();
        }

        let row = self.nth_surviving(from, remaining);
        (row < self.total_rows).then_some(row)
    }

    /// The `n`th row at or after `from` that has not been struck out.
    fn nth_surviving(&self, from: usize, n: usize) -> usize {
        let mut row = from + n;
        for struck in self.overlay.struck() {
            if struck < from {
                continue;
            }
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
            Source::Lazy(_) | Source::Json(_) => {
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
        // Added rows are not in the file, so they are not fetched — a blank
        // row stands in for each, and the overlay fills it in afterwards the
        // same way it fills in an edited cell.
        if wanted.iter().any(|&row| self.is_added(row)) {
            return self.rows_with_added(wanted);
        }
        let (Some(&first), Some(&last)) = (wanted.first(), wanted.last()) else {
            // No rows, but the caller still needs the right columns.
            return self.empty_page();
        };
        let span = self.fetch_span(first, last - first + 1)?;
        if wanted.len() == last - first + 1 {
            return Ok(span);
        }
        let picked: Vec<IdxSize> = wanted.iter().map(|&row| (row - first) as IdxSize).collect();
        Ok(span.take(&IdxCa::from_vec(PlSmallStr::from_static("i"), picked))?)
    }

    /// A page of the file's rows with blanks left where rows were added.
    ///
    /// Built by stacking runs of fetched rows and blanks in display order,
    /// rather than a frame per row: a page has a handful of added rows at
    /// most, so this is a handful of pieces.
    fn rows_with_added(&self, wanted: &[usize]) -> Result<DataFrame> {
        let from_file: Vec<usize> = wanted
            .iter()
            .copied()
            .filter(|&row| !self.is_added(row))
            .collect();
        let fetched = self.rows_at(&from_file)?;

        let mut page: Option<DataFrame> = None;
        let mut taken = 0usize;
        let mut run = 0usize;
        let stack = |page: &mut Option<DataFrame>, piece: DataFrame| -> Result<()> {
            match page {
                Some(so_far) => so_far.vstack_mut(&piece).map(|_| ())?,
                None => *page = Some(piece),
            }
            Ok(())
        };

        for &row in wanted {
            if self.is_added(row) {
                if run > 0 {
                    stack(&mut page, fetched.slice(taken as i64, run))?;
                    taken += run;
                    run = 0;
                }
                stack(&mut page, self.blank_row()?)?;
            } else {
                run += 1;
            }
        }
        if run > 0 {
            stack(&mut page, fetched.slice(taken as i64, run))?;
        }
        match page {
            Some(page) => Ok(page),
            None => self.empty_page(),
        }
    }

    /// One row of nulls, shaped like the page.
    fn blank_row(&self) -> Result<DataFrame> {
        let columns: Vec<Column> = self
            .columns()
            .into_iter()
            .filter_map(|source| self.schema.get_at_index(source))
            .map(|(name, dtype)| Column::full_null(name.clone(), 1, dtype))
            .collect();
        Ok(DataFrame::new(1, columns)?)
    }

    /// No rows, but the columns the caller is going to draw.
    fn empty_page(&self) -> Result<DataFrame> {
        match self.effective_lf() {
            Some(lf) => Ok(lf.slice(0, 0).collect()?),
            None => self.blank_row().map(|df| df.slice(0, 0)),
        }
    }

    /// Rows `first..first + span` of the source.
    ///
    /// Read from the byte offset the index points at when there is one and
    /// nothing is composed on top — a sort or a projection changes what a row
    /// number means, so those go the lazy way.
    fn fetch_span(&self, first: usize, span: usize) -> Result<DataFrame> {
        // A JSONL page is built here or nowhere: there is no frame to slice,
        // and the parse can apply the projection itself, so a `:select` is no
        // reason to leave this path the way it is for a delimited file.
        if matches!(self.source, Source::Json(_)) {
            return match self.indexed_span(first, span) {
                Some(page) => page,
                None => self.empty_page(),
            };
        }
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
        let (path, records) = self.records()?;
        if offset >= index.rows() {
            return None;
        }
        let (first_row, from) = index.seek(offset);
        let to = index.end_of(offset + height);
        // Only the columns on show are built for a JSONL page. A delimited
        // page parses whole and is projected by the frame above it, which is
        // why that path stands aside for a `:select` and this one need not.
        let wanted = self.columns();

        Some((|| {
            let bytes = index::read_span(&path, index, from, to)?;
            // The span starts at a checkpoint, which is at or before the page.
            records.parse(bytes, &self.schema, &wanted, offset - first_row, height)
        })())
    }

    /// The file the rows are read out of and how its records are parsed, for
    /// the two paths that read the file themselves: a page, and a scan.
    fn records(&self) -> Option<(PathBuf, Records)> {
        match &self.source {
            Source::Json(file) => Some((
                file.path.clone(),
                Records::Json(Arc::new(file.columns.clone())),
            )),
            _ => self.edit.as_ref().and_then(|target| match target.written {
                Written::Delimited(separator) => {
                    Some((target.path.clone(), Records::Delimited(separator)))
                }
                // A JSONL file is always `Source::Json`, matched above.
                Written::Json => None,
            }),
        }
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
            // The same held frame as a delimited file's, and the same
            // `materialise` after it — only the reading is different, because
            // there is no `LazyFrame` to start from. Building it is a full
            // parse of the file, which is why it happens on the worker rather
            // than in front of the user.
            Source::Json(file) => {
                let path = file.path.clone();
                let paths = file.columns.clone();
                let Some(index) = self.row_index.clone() else {
                    return rx;
                };
                let schema = self.schema.clone();
                let filter = self.view.filter.clone();
                let bytes = self.scan_bytes.unwrap_or_else(budget::scan_bytes);
                thread::spawn(move || {
                    // Built on the worker: an `Expr` is not `Send`.
                    let predicate = filter.and_then(|f| rows::predicate(&f, &schema));
                    let Ok(frame) = json_frame(&path, &index, &schema, &paths, bytes) else {
                        return;
                    };
                    if let Ok(df) = Self::materialise(frame.lazy(), &keys, predicate) {
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
        // Lake tables sort through DuckDB, which spills to disk; everything
        // plv sorts itself has to fit in memory, JSONL included — it is the
        // same held frame, only built by a different reader.
        if !matches!(self.source, Source::Lake(_)) && !self.sort_fits() {
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
                Source::Json(_) | Source::Lazy(_) => {
                    "only csv, tsv, tab, txt and jsonl files can be edited"
                }
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

    /// Why a row cannot be added here, or `None` when it can.
    pub fn insert_blocked(&self) -> Option<&'static str> {
        self.delete_blocked()
    }

    /// Add an empty row next to the one at `display`, and say where it landed.
    ///
    /// A new row belongs *before* some row of the file, and among any others
    /// already added there — which is what keeps `o` and `O` meaning below and
    /// above even when the neighbour is itself a new row.
    pub fn add_row(&mut self, display: usize, below: bool) -> Result<usize> {
        let (before, at) = match self.source_row(display) {
            Some(row) if self.is_added(row) => {
                let (anchor, place) = self.overlay.locate(row).unwrap_or((self.total_rows, 0));
                (anchor, if below { place + 1 } else { place })
            }
            Some(row) if below => (row + 1, 0),
            Some(row) => (row, self.overlay.added_at(row).len()),
            // An empty file, or the cursor past the end: it goes on the end.
            None => (
                self.total_rows,
                self.overlay.added_at(self.total_rows).len(),
            ),
        };
        self.overlay.add_row(before, at, self.total_rows);
        self.refresh()?;
        Ok(if below && self.row_count() > 1 {
            display + 1
        } else {
            display
        })
    }

    /// Strike out the rows at these display positions, as one undoable step.
    pub fn delete_rows<I: IntoIterator<Item = usize>>(&mut self, rows: I) -> Result<usize> {
        let struck: Vec<usize> = rows
            .into_iter()
            .filter_map(|d| self.source_row(d))
            .collect();
        let count = struck.len();
        self.overlay.delete(struck, self.total_rows);
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
        let written = target.written;
        let dst = dst.unwrap_or(&src).to_path_buf();

        let stamp = match written {
            // plv always reads a header row, so record 0 is never data.
            Written::Delimited(separator) => {
                writer::save(&src, &dst, separator, true, &self.overlay, self.total_rows)?
            }
            Written::Json => writer::save_json(
                &src,
                &dst,
                &self.json_columns(),
                &self.overlay,
                self.total_rows,
            )?,
        };

        if dst == src {
            self.overlay.clear();
            if let Some(target) = &mut self.edit {
                target.stamp = stamp;
            }
            self.reload()?;
        }
        Ok(dst)
    }

    /// Where each column lives in a record and what it may be written as.
    ///
    /// The paths are the ones pages are read through, so an edit lands in the
    /// bytes the cell was read from — an `:expand`ed column included, which is
    /// what lets a nested value be edited at all.
    fn json_columns(&self) -> Vec<writer::JsonColumn> {
        let Source::Json(file) = &self.source else {
            return Vec::new();
        };
        file.columns
            .iter()
            .enumerate()
            .map(|(at, path)| writer::JsonColumn {
                path: path.clone(),
                kind: match self.schema.get_at_index(at).map(|(_, dtype)| dtype) {
                    Some(DataType::Int64) => writer::JsonType::Int,
                    Some(DataType::Float64) => writer::JsonType::Float,
                    Some(DataType::Boolean) => writer::JsonType::Bool,
                    _ => writer::JsonType::Text,
                },
            })
            .collect()
    }

    /// Re-open the file after writing to it, in case an edit changed a
    /// column's inferred type. The row count cannot have changed: a value
    /// containing a newline is quoted, so it stays one record.
    fn reload(&mut self) -> Result<()> {
        let Some(target) = &self.edit else {
            return Ok(());
        };
        let path = target.path.clone();
        if matches!(target.written, Written::Json) {
            return self.reload_json(&path);
        }
        let mut lf = loader::load(&path)?;
        self.schema = lf.collect_schema()?;
        self.source = Source::Lazy(lf);
        // The held sort describes the file as it was before the write.
        self.sorted = None;
        self.refresh()
    }

    /// Re-open a JSONL file after writing to it.
    ///
    /// More than the delimited reload does, because more can have changed: an
    /// edit can widen a column's type, and a deleted or added record changes
    /// both the row count and where every record after it begins. So the
    /// index, the count and the keys are all read again — the same one pass
    /// that opens the file.
    ///
    /// The view is put back **by name**, since the column numbers are only
    /// meaningful against the schema they were written for, and any column
    /// `:expand` made is re-appended: it is derived from the file rather than
    /// found in it, so a re-read would otherwise quietly drop it along with
    /// the edit that was just written through it.
    fn reload_json(&mut self, path: &Path) -> Result<()> {
        let expanded: Vec<(DataType, jsonl::KeyPath)> = match &self.source {
            Source::Json(file) => file
                .columns
                .iter()
                .enumerate()
                .filter(|(_, path)| path.len() > 1)
                .filter_map(|(at, path)| {
                    let (_, dtype) = self.schema.get_at_index(at)?;
                    Some((dtype.clone(), path.clone()))
                })
                .collect(),
            _ => Vec::new(),
        };
        let shown: Option<Vec<String>> = self.view.select.as_ref().map(|cols| {
            cols.iter()
                .filter_map(|&at| self.schema.get_at_index(at))
                .map(|(name, _)| name.to_string())
                .collect()
        });

        let mut scan = jsonl::Scan::new();
        let index = RowIndex::build_lines(path, &mut |byte| scan.byte(byte))?;
        let fields = scan.finish();
        let mut schema = (*fields.schema()).clone();
        let mut paths = fields.paths();
        for (dtype, path) in expanded {
            // Its parent may have been deleted along with the last record
            // that carried it.
            if !schema.contains(&path[0][..]) {
                continue;
            }
            let name = unique_name(&schema, path.join("."));
            schema.with_column(name.into(), dtype);
            paths.push(path);
        }

        self.schema = Arc::new(schema);
        self.source = Source::Json(JsonFile {
            path: path.to_path_buf(),
            columns: paths,
        });
        self.total_rows = index.rows();
        self.row_index = Some(std::sync::Arc::new(index));
        // The held sort describes the file as it was before the write.
        self.sorted = None;
        self.view.select = shown
            .map(|names| {
                names
                    .iter()
                    .filter_map(|name| self.schema.index_of(&name[..]))
                    .collect::<Vec<_>>()
            })
            .filter(|cols: &Vec<usize>| !cols.is_empty());
        self.row_offset = self.row_offset.min(self.row_count().saturating_sub(1));
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
            Source::Lazy(_) | Source::Json(_) => self.search_lazy(pattern, col_name, tx),
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
        // the lazy one when the view has not changed what a row is. A JSONL
        // page is parsed whole for a scan whatever the view shows, and cannot
        // be sorted at all, so a `:select` is no reason to stand aside.
        let projected = self.view.select.is_some() && !matches!(self.source, Source::Json(_));
        let plain = self.sorted.is_none() && self.view.sort.is_empty() && !projected;
        if plain && self.scan_indexed(tx.clone(), build).is_some() {
            return;
        }

        // A held sort is both the frame on screen and much the faster thing to
        // scan; without one, fall back to the lazy pipeline — which a JSONL
        // file has none of, so for it the indexed scan is the only scan.
        let (lf, total) = match &self.sorted {
            Some(sorted) => (sorted.clone().lazy(), sorted.height()),
            None => match self.effective_lf() {
                Some(lf) => (lf, self.total_rows),
                None => return,
            },
        };
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
        let (path, records) = self.records()?;
        // A predicate names source columns, so a scanned chunk is parsed
        // whole however narrow the view is.
        let wanted: Vec<usize> = (0..self.schema.len()).collect();
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
                // A scan wants the whole chunk, so it takes it from the top.
                let Ok(chunk) = records.parse(bytes, &schema, &wanted, 0, usize::MAX) else {
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
        // The scan runs on the base frame, so the indices it reports are the
        // file's own rows. That is what makes them usable as edit-buffer keys.
        // A JSONL file has no frame: the indexed scan is the only scan, and
        // there is nothing to fall back to.
        let lf = match &self.source {
            Source::Lazy(base) => Some(base.clone()),
            Source::Json(_) => None,
            Source::Lake(_) => return Ok(None),
        };
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
            let Some(lf) = lf else {
                // Nothing is going to fill the set, so it must not be left
                // looking like a filter that is still resolving.
                self.filter_rows = None;
                self.refresh()?;
                return Ok(None);
            };
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

/// A name no column has yet.
///
/// Only reachable by a record holding both `err` and a literal `err.code` at
/// the top level, and then only when the first is expanded. Rare enough to
/// deserve a suffix rather than a refusal — the name still says where the
/// column came from, and no data is lost.
fn unique_name(schema: &Schema, wanted: String) -> String {
    if !schema.contains(&wanted[..]) {
        return wanted;
    }
    (2..)
        .map(|n| format!("{wanted}#{n}"))
        .find(|name| !schema.contains(&name[..]))
        .expect("an unused name")
}

/// The whole of a JSONL file as one frame, read in chunks bounded by bytes.
///
/// The only way to sort one: a sort has to see every row, and there is no
/// lazy frame here to hand that job to. Chunked by the same byte budget the
/// filter scan uses, and read from the offsets the index points at, so the
/// cost is one linear pass rather than a re-read per chunk. Whether the
/// result will fit is `sort_blocked`'s question, asked before this is called.
fn json_frame(
    path: &Path,
    index: &RowIndex,
    schema: &SchemaRef,
    paths: &[jsonl::KeyPath],
    bytes: u64,
) -> Result<DataFrame> {
    let wanted: Vec<usize> = (0..schema.len()).collect();
    let total = index.rows();
    let mut frame: Option<DataFrame> = None;
    let mut start = 0usize;

    while start < total {
        let end = index.chunk_end(start, bytes);
        let (first_row, from) = index.seek(start);
        let to = index.end_of(end);
        let chunk = jsonl::page(
            &index::read_span(path, index, from, to)?,
            schema,
            paths,
            &wanted,
            start - first_row,
            end - start,
        )?;
        match &mut frame {
            Some(so_far) => {
                so_far.vstack_mut(&chunk)?;
            }
            None => frame = Some(chunk),
        }
        start = end;
    }

    match frame {
        Some(mut frame) => {
            // One run of chunks to sort rather than a few hundred stacked
            // ones, each column aligned with the rest.
            frame.align_chunks_par();
            Ok(frame)
        }
        None => Ok(DataFrame::empty()),
    }
}

/// How the bytes of a span become rows.
///
/// The one place the two file kinds differ once the index has said which bytes
/// to read — everything above this treats them the same.
#[derive(Clone)]
enum Records {
    Delimited(u8),
    /// The paths of every column, shared rather than copied: a scan hands
    /// them to a worker thread and a page reads them on this one.
    Json(Arc<Vec<jsonl::KeyPath>>),
}

impl Records {
    /// Rows `skip..skip + take` of a span, as the columns `wanted` names.
    ///
    /// A span begins at the checkpoint before the page, which can be a whole
    /// stride earlier, so the window is named here rather than sliced off
    /// afterwards: the JSONL reader can then walk past the records ahead of
    /// the page instead of building them.
    fn parse(
        &self,
        bytes: Vec<u8>,
        schema: &SchemaRef,
        wanted: &[usize],
        skip: usize,
        take: usize,
    ) -> Result<DataFrame> {
        match self {
            Records::Delimited(separator) => {
                Ok(parse_span(bytes, schema, *separator)?.slice(skip as i64, take))
            }
            Records::Json(paths) => jsonl::page(&bytes, schema, paths, wanted, skip, take),
        }
    }
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

    /// A small log: every record has `ts`, `level` and `msg`, `err` is on the
    /// one that failed, and `err` is a document rather than a value.
    fn log_file(name: &str) -> PathBuf {
        let mut text = String::new();
        for i in 0..8 {
            text.push_str(&format!(
                "{{\"ts\":\"10:00:0{i}\",\"level\":\"info\",\"msg\":\"tick\",\"n\":{i}}}\n"
            ));
        }
        text.push_str(
            "{\"ts\":\"10:00:08\",\"level\":\"error\",\"msg\":\"boom\",\"n\":8,\"err\":{\"code\":500}}\n",
        );
        write_temp(name, &text)
    }

    #[test]
    fn a_jsonl_file_opens_with_its_keys_as_columns() {
        let store = Store::open_file(&log_file("log-open.jsonl"), 4).unwrap();
        assert_eq!(store.total_rows, 9);
        assert_eq!(
            store
                .schema
                .iter_names()
                .map(|n| n.to_string())
                .collect::<Vec<_>>(),
            ["ts", "level", "msg", "n", "err"],
            "most common first"
        );
        // `n` was whole numbers throughout, so it is a number and not text.
        assert_eq!(store.schema.get_at_index(3).unwrap().1, &DataType::Int64);

        // `err` is on one record in nine, which is under the share that gets
        // shown — it is a column, it is simply not on screen.
        assert_eq!(store.view.select, Some(vec![0, 1, 2, 3]));
        assert!(store.notes().unwrap().contains("4 of 5 keys shown"));
        assert_eq!(store.current_view.width(), 4);
        assert_eq!(cell(&store.current_view, 2, 0).as_deref(), Some("tick"));
    }

    #[test]
    fn a_jsonl_page_is_read_from_the_byte_offset_the_index_points_at() {
        let mut store = Store::open_file(&log_file("log-page.jsonl"), 3).unwrap();
        store.scroll_to_offset(6).unwrap();
        assert_eq!(cell(&store.current_view, 0, 0).as_deref(), Some("10:00:06"));
        assert_eq!(cell(&store.current_view, 3, 2).as_deref(), Some("8"));
    }

    /// The point of the whole exercise: a nested value reaches the cell as the
    /// file's own bytes, so the cell window can open it as a document.
    #[test]
    fn a_nested_value_survives_to_the_cell_as_json() {
        let mut store = Store::open_file(&log_file("log-nested.jsonl"), 10).unwrap();
        // Show every key, the way `:reset select` does.
        store.apply_view(View::default()).unwrap();
        let err = store.schema.len() - 1;
        let text = store.cell_text(8, err).unwrap();
        assert_eq!(text, "{\"code\":500}");
        assert!(crate::ui::json::reindent(&text).is_some());
    }

    /// A JSONL file has no lazy frame to fall back to, so the indexed scan is
    /// the only scan — and `:filter` has to reach it.
    #[test]
    fn a_jsonl_file_filters_through_the_indexed_scan() {
        let path = log_file("log-filter.jsonl");
        let mut store = Store::open_file(&path, 4).unwrap();
        store.view.filter = Some(
            match crate::view::parse("filter level = error", &store.schema).unwrap() {
                crate::view::Command::Filter(filter) => filter,
                other => panic!("{other:?}"),
            },
        );
        let rx = store.begin_filter().unwrap().expect("a scan was started");
        while let Ok(batch) = rx.recv_timeout(std::time::Duration::from_secs(30)) {
            if !store.extend_filter(batch).unwrap() {
                break;
            }
        }
        store.finish_filter().unwrap();

        assert_eq!(store.row_count(), 1);
        assert_eq!(store.source_row(0), Some(8), "the row it came from");
        assert_eq!(cell(&store.current_view, 2, 0).as_deref(), Some("boom"));
    }

    /// A typed column is typed all the way to the view language: `n` was whole
    /// numbers in every record, so it can be compared with `>`.
    #[test]
    fn a_number_key_can_be_filtered_as_a_number() {
        let path = log_file("log-typed.jsonl");
        let store = Store::open_file(&path, 4).unwrap();
        assert!(
            crate::view::parse("filter n > 6", &store.schema).is_ok(),
            "an int column takes an int literal"
        );
        assert!(
            crate::view::parse("filter n > abc", &store.schema).is_err(),
            "and refuses one that is not"
        );
    }

    /// The interesting page is one found by seeking to a checkpoint rather
    /// than by counting from the top — which needs a file bigger than one
    /// stride, and is exactly where an off-by-one in the line scan would hide.
    #[test]
    fn a_jsonl_page_past_a_checkpoint_lands_on_the_right_rows() {
        use crate::data::index::STRIDE;

        let rows = STRIDE + 100;
        let mut text = String::with_capacity(rows * 24);
        for i in 0..rows {
            text.push_str(&format!("{{\"i\":{i},\"s\":\"r{i}\"}}\n"));
        }
        let path = write_temp("log-stride.jsonl", &text);
        let mut store = Store::open_file(&path, 5).unwrap();
        assert_eq!(store.total_rows, rows);

        let last = rows - 5;
        store.scroll_to_offset(last).unwrap();
        assert_eq!(
            cell(&store.current_view, 0, 0).as_deref(),
            Some(&*last.to_string())
        );
        assert_eq!(
            cell(&store.current_view, 1, 4).as_deref(),
            Some(&*format!("r{}", rows - 1)),
            "the last row of the file"
        );

        // And a page that straddles the checkpoint itself.
        store.scroll_to_offset(STRIDE - 2).unwrap();
        assert_eq!(
            cell(&store.current_view, 0, 3).as_deref(),
            Some(&*(STRIDE + 1).to_string())
        );
    }

    #[test]
    fn expand_lifts_a_document_into_columns_where_its_parent_stood() {
        let mut store = Store::open_file(&log_file("log-expand.jsonl"), 10).unwrap();
        // `err` is rare, so it opens hidden; show everything first.
        store.apply_view(View::default()).unwrap();
        let err = store.schema.len() - 1;

        let said = store.expand(err).unwrap();
        assert!(said.contains("err expanded into 1 column"), "{said}");

        let names: Vec<String> = store.schema.iter_names().map(|n| n.to_string()).collect();
        assert_eq!(
            names,
            ["ts", "level", "msg", "n", "err", "err.code"],
            "appended, so every column index still means what it did"
        );
        assert_eq!(
            store.schema.get("err.code"),
            Some(&DataType::Int64),
            "and typed by what was inside it"
        );

        // The child stands where its parent stood, and the parent is still a
        // column — just not on show.
        let shown: Vec<String> = store
            .current_view
            .get_column_names()
            .iter()
            .map(|n| n.to_string())
            .collect();
        assert_eq!(shown, ["ts", "level", "msg", "n", "err.code"]);
        assert_eq!(cell(&store.current_view, 4, 8).as_deref(), Some("500"));
        assert!(store.schema.contains("err"), "the document is still there");
    }

    #[test]
    fn expand_refuses_what_holds_no_documents() {
        let mut store = Store::open_file(&log_file("log-expand-flat.jsonl"), 10).unwrap();
        let refusal = store.expand(2).unwrap_err().to_string(); // `msg`
        assert!(refusal.contains("no documents in msg"), "{refusal}");

        store.apply_view(View::default()).unwrap();
        let err = store.schema.len() - 1;
        store.expand(err).unwrap();
        let again = store.expand(err).unwrap_err().to_string();
        assert!(again.contains("already expanded"), "{again}");
    }

    #[test]
    fn a_jsonl_file_can_be_edited_and_sorted() {
        let store = Store::open_file(&log_file("log-blocked.jsonl"), 4).unwrap();
        assert_eq!(store.edit_blocked(), None);
        assert!(store.sort_blocked().is_none());
    }

    /// An edit replaces the value's bytes and nothing else: key order,
    /// spacing and every other record survive exactly.
    #[test]
    fn writing_a_jsonl_edit_touches_only_that_value() {
        let path = write_temp(
            "log-write.jsonl",
            concat!(
                r#"{"ts": "10:00:00", "level":"info","n":1}"#,
                "\n",
                r#"{"ts": "10:00:01", "level":"error","n":2}"#,
                "\n"
            ),
        );
        let mut store = Store::open_file(&path, 10).unwrap();
        store.edit([((1, 1), "warn".to_string())]).unwrap();
        store.save(None, false).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            concat!(
                r#"{"ts": "10:00:00", "level":"info","n":1}"#,
                "\n",
                r#"{"ts": "10:00:01", "level":"warn","n":2}"#,
                "\n"
            ),
            "the spacing of the untouched record is its own"
        );
        assert_eq!(store.dirty(), 0, "and the buffer is clean afterwards");
    }

    #[test]
    fn a_number_is_written_as_a_number_and_anything_else_as_text() {
        let path = write_temp(
            "log-types.jsonl",
            "{\"n\":1,\"b\":true}\n{\"n\":2,\"b\":false}\n",
        );
        let mut store = Store::open_file(&path, 10).unwrap();
        store.edit([((0, 0), "42".to_string())]).unwrap();
        store.edit([((1, 1), "true".to_string())]).unwrap();
        store.save(None, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"n\":42,\"b\":true}\n{\"n\":2,\"b\":true}\n"
        );

        // A value the column cannot hold is written as text rather than
        // refused, and the column widens when the file is read again.
        let mut store = Store::open_file(&path, 10).unwrap();
        store.edit([((0, 0), "n/a".to_string())]).unwrap();
        store.save(None, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"n\":\"n/a\",\"b\":true}\n{\"n\":2,\"b\":true}\n"
        );
        assert_eq!(store.schema.get_at_index(0).unwrap().1, &DataType::String);
    }

    #[test]
    fn a_key_the_record_lacks_is_added_and_an_empty_edit_is_null() {
        let path = write_temp("log-add-key.jsonl", "{\"a\":1,\"b\":2}\n{\"a\":3}\n");
        let mut store = Store::open_file(&path, 10).unwrap();
        store.edit([((1, 1), "9".to_string())]).unwrap(); // `b`, which record 1 lacks
        store.edit([((0, 1), String::new())]).unwrap(); // cleared
        store.save(None, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"a\":1,\"b\":null}\n{\"a\":3,\"b\":9}\n"
        );
    }

    /// Editing an `:expand`ed column writes into the document it came out of.
    #[test]
    fn an_expanded_column_writes_back_into_its_document() {
        let path = write_temp(
            "log-nested-write.jsonl",
            "{\"err\":{\"code\":500,\"why\":\"boom\"}}\n{\"err\":{\"code\":502,\"why\":\"gone\"}}\n",
        );
        let mut store = Store::open_file(&path, 10).unwrap();
        store.expand(0).unwrap();
        // `edit` counts columns the way the screen does, and after an expand
        // the children stand where their parent stood.
        let code = store
            .display_column(store.schema.index_of("err.code").unwrap())
            .unwrap();
        store.edit([((0, code), "503".to_string())]).unwrap();
        store.save(None, false).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"err\":{\"code\":503,\"why\":\"boom\"}}\n{\"err\":{\"code\":502,\"why\":\"gone\"}}\n",
            "only the number changed"
        );
        // The re-read keeps the expansion, or the edit would have written
        // through a column that then vanished.
        assert!(store.schema.contains("err.code"), "{:?}", store.schema);
    }

    /// Adding a key one level in would mean inventing the documents above it,
    /// which is a bigger decision than an edit. Refused, and the file is left
    /// exactly as it was.
    #[test]
    fn writing_refuses_to_invent_a_document_for_a_nested_key() {
        let original = "{\"err\":{\"code\":1}}\n{\"err\":{\"why\":\"x\"}}\n";
        let path = write_temp("log-nested-refuse.jsonl", original);
        let mut store = Store::open_file(&path, 10).unwrap();
        store.expand(0).unwrap();
        let code = store
            .display_column(store.schema.index_of("err.code").unwrap())
            .unwrap();

        store.edit([((1, code), "7".to_string())]).unwrap();
        let refusal = store.save(None, false).unwrap_err().to_string();
        assert!(refusal.contains("no key to land in"), "{refusal}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "and nothing was written"
        );
        assert_eq!(store.dirty(), 1, "the edit is still pending");
    }

    #[test]
    fn deleting_and_adding_records_reaches_the_file() {
        let path = write_temp("log-rows.jsonl", "{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n");
        let mut store = Store::open_file(&path, 10).unwrap();
        store.delete_rows([1usize]).unwrap();
        store.save(None, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"a\":1}\n{\"a\":3}\n"
        );
        assert_eq!(store.row_count(), 2, "and the view knows the file shrank");

        let added = store.add_row(0, true).unwrap();
        store.edit([((added, 0), "9".to_string())]).unwrap();
        store.save(None, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"a\":1}\n{\"a\":9}\n{\"a\":3}\n"
        );
        assert_eq!(store.row_count(), 3);
    }

    /// Escapes are the file's own on the way in and plv's on the way out, and
    /// a value that came back unchanged has to land byte for byte.
    #[test]
    fn escapes_survive_a_round_trip() {
        let original = "{\"m\":\"a \\\"quote\\\" and a \\n\",\"n\":1}\n";
        let path = write_temp("log-escapes.jsonl", original);
        let mut store = Store::open_file(&path, 10).unwrap();
        let read = store.cell_text(0, 0).unwrap();
        assert_eq!(read, "a \"quote\" and a \n");

        store.edit([((0, 0), read)]).unwrap();
        store.save(None, false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    /// Sorting reads the file once into a held frame, exactly as a delimited
    /// file's sort does — the reading is all that differs.
    fn sorted_by(store: &mut Store, display_col: usize) {
        let rx = store.begin_sort(display_col);
        let df = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the sort produced a frame");
        store.adopt_sorted(df).unwrap();
    }

    #[test]
    fn a_jsonl_file_sorts_by_holding_the_whole_table() {
        let mut store = Store::open_file(&log_file("log-sort.jsonl"), 10).unwrap();
        sorted_by(&mut store, 1); // `level`

        assert_eq!(cell(&store.current_view, 1, 0).as_deref(), Some("error"));
        assert_eq!(
            store.source_row(0),
            Some(8),
            "and the sorted row still knows which line it came from"
        );
        assert_eq!(store.row_count(), 9, "every row is still there");

        // Descending on the second press, as it does everywhere else.
        sorted_by(&mut store, 1);
        assert_eq!(cell(&store.current_view, 1, 0).as_deref(), Some("info"));
    }

    /// A held sort carries the filter, rather than a row-set scan running
    /// beside it over a file that is already in memory.
    #[test]
    fn a_jsonl_sort_and_filter_compose() {
        let mut store = Store::open_file(&log_file("log-sortfilter.jsonl"), 10).unwrap();
        let mut view = store.view.clone();
        view.filter = Some(
            match crate::view::parse("filter n > 6", &store.schema).unwrap() {
                crate::view::Command::Filter(filter) => filter,
                other => panic!("{other:?}"),
            },
        );
        store.apply_view(view).unwrap();
        sorted_by(&mut store, 3); // `n`

        assert_eq!(store.row_count(), 2, "rows 7 and 8");
        assert_eq!(cell(&store.current_view, 3, 0).as_deref(), Some("7"));
        assert_eq!(store.source_row(1), Some(8));
    }

    /// The interesting sort is one whose read crosses a chunk seam, since that
    /// is where the row numbering would go wrong.
    #[test]
    fn a_jsonl_sort_reads_the_whole_file_across_its_chunks() {
        let rows = 5_000;
        let mut text = String::new();
        for i in 0..rows {
            text.push_str(&format!(
                "{{\"i\":{},\"pad\":\"{}\"}}\n",
                rows - i,
                "x".repeat(40)
            ));
        }
        let path = write_temp("log-sortchunks.jsonl", &text);
        let mut store = Store::open_file(&path, 5).unwrap();
        // Force several chunks out of a small fixture.
        store.scan_bytes = Some(16 << 10);
        sorted_by(&mut store, 0); // `i`, which counts down the file

        assert_eq!(store.row_count(), rows);
        assert_eq!(cell(&store.current_view, 0, 0).as_deref(), Some("1"));
        assert_eq!(
            store.source_row(0),
            Some(rows - 1),
            "the last line of the file sorts first"
        );
    }

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
