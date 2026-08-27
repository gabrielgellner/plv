use std::sync::mpsc;
use std::thread::{self, yield_now};

use anyhow::Result;
use polars::prelude::*;

pub struct Store {
    base_lf: LazyFrame,
    pub schema: SchemaRef,
    pub total_rows: usize,
    pub row_offset: usize,
    pub viewport_rows: usize,
    pub current_view: DataFrame,
    /// Active sort keys in priority order: `(column_index, ascending)`.
    /// Empty = natural order. First entry is the primary sort key.
    pub sort: Vec<(usize, bool)>,
}

impl Store {
    /// Open a store, counting rows by scanning. Prefer
    /// [`Store::with_row_count`] when the row count is already known.
    pub fn new(lf: LazyFrame, viewport_rows: usize) -> Result<Self> {
        Self::build(lf, viewport_rows, None)
    }

    /// Open a store with a row count supplied by the caller.
    ///
    /// A DuckLake catalog records an exact `record_count` per data file, so
    /// counting again would mean a full scan of every file — seconds of
    /// startup lag on a billion-row table, for a number we already have.
    pub fn with_row_count(lf: LazyFrame, viewport_rows: usize, total_rows: usize) -> Result<Self> {
        Self::build(lf, viewport_rows, Some(total_rows))
    }

    fn build(mut lf: LazyFrame, viewport_rows: usize, total_rows: Option<usize>) -> Result<Self> {
        let schema = lf.collect_schema()?;
        let total_rows = match total_rows {
            Some(n) => n,
            None => Self::count_rows(&lf)?,
        };
        let current_view = Self::fetch(&lf, 0, viewport_rows)?;
        Ok(Self {
            base_lf: lf,
            schema,
            total_rows,
            row_offset: 0,
            viewport_rows,
            current_view,
            sort: Vec::new(),
        })
    }

    /// Effective lazy frame: base with all sort keys applied in priority order.
    fn effective_lf(&self) -> LazyFrame {
        if self.sort.is_empty() {
            return self.base_lf.clone();
        }
        let (names, descending): (Vec<String>, Vec<bool>) = self
            .sort
            .iter()
            .filter_map(|&(ci, asc)| {
                self.schema
                    .get_at_index(ci)
                    .map(|(name, _)| (name.to_string(), !asc))
            })
            .unzip();
        if names.is_empty() {
            return self.base_lf.clone();
        }
        self.base_lf.clone().sort(
            names,
            SortMultipleOptions::default().with_order_descending_multi(descending),
        )
    }

    /// Toggle sort direction on `col_idx`, or add it as a new ascending sort key.
    /// Updates sort state immediately and spawns a background thread to fetch
    /// the new first page. The caller should replace `current_view` when the
    /// DataFrame arrives on the returned receiver.
    pub fn begin_sort(&mut self, col_idx: usize) -> mpsc::Receiver<DataFrame> {
        if let Some(entry) = self.sort.iter_mut().find(|(ci, _)| *ci == col_idx) {
            entry.1 = !entry.1;
        } else {
            self.sort.push((col_idx, true));
        }
        self.row_offset = 0;
        let lf = self.effective_lf();
        let vp = self.viewport_rows;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            if let Ok(df) = Self::fetch(&lf, 0, vp) {
                let _ = tx.send(df);
            }
        });
        rx
    }

    /// Clear all sort keys and return to natural order.
    pub fn clear_sort(&mut self) -> Result<()> {
        self.sort.clear();
        self.row_offset = 0;
        self.current_view = Self::fetch(&self.base_lf, 0, self.viewport_rows)?;
        Ok(())
    }

    pub fn scroll_to_offset(&mut self, offset: usize) -> Result<()> {
        let max = self.total_rows.saturating_sub(self.viewport_rows);
        self.row_offset = offset.min(max);
        let lf = self.effective_lf();
        self.current_view = Self::fetch(&lf, self.row_offset, self.viewport_rows)?;
        Ok(())
    }

    pub fn resize(&mut self, new_height: usize) -> Result<()> {
        if self.viewport_rows != new_height && new_height > 0 {
            self.viewport_rows = new_height;
            let lf = self.effective_lf();
            self.current_view = Self::fetch(&lf, self.row_offset, self.viewport_rows)?;
        }
        Ok(())
    }

    fn fetch(lf: &LazyFrame, offset: usize, height: usize) -> Result<DataFrame> {
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
        let lf = self.effective_lf();
        let schema = self.schema.clone();
        let total = self.total_rows;

        thread::spawn(move || {
            // Build the filter expression inside the thread so no non-Send Expr
            // crosses a thread boundary.
            let filter = if let Some(ref name) = col_name {
                col(name.as_str())
                    .cast(DataType::String)
                    .str()
                    .contains(lit(pattern.as_str()), false)
            } else {
                let f = schema
                    .iter_names()
                    .map(|name| {
                        col(name.as_str())
                            .cast(DataType::String)
                            .str()
                            .contains(lit(pattern.as_str()), false)
                    })
                    .reduce(|acc: Expr, e: Expr| acc.or(e));
                let Some(f) = f else { return };
                f
            };

            const CHUNK: usize = 10_000;
            let mut offset = 0usize;

            while offset < total {
                let size = CHUNK.min(total - offset);

                let Ok(df) = lf
                    .clone()
                    .slice(offset as i64, size as u32)
                    .with_row_index("__idx__", Some(offset as u32))
                    .filter(filter.clone())
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
                    return; // receiver dropped — search cancelled
                }

                offset += CHUNK;

                // Yield between chunks so the main thread's scroll queries
                // can interleave with the background search without lag.
                yield_now();
            }
        });
    }

    fn count_rows(lf: &LazyFrame) -> Result<usize> {
        let df = lf.clone().select([len().alias("n")]).collect()?;
        Ok(df.column("n")?.u32()?.get(0).unwrap_or(0) as usize)
    }
}
