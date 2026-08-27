use std::sync::mpsc;
use std::thread::{self, yield_now};

use anyhow::Result;
use duckdb::Connection;
use polars::prelude::*;

use crate::data::lake_db::{self, LakeSource};

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

pub struct Store {
    source: Source,
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
            sort: Vec::new(),
        })
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
            sort: Vec::new(),
        })
    }

    /// Sort keys as `(column_name, ascending)`, dropping any stale indices.
    fn sort_keys(&self) -> Vec<(String, bool)> {
        self.sort
            .iter()
            .filter_map(|&(ci, asc)| {
                self.schema
                    .get_at_index(ci)
                    .map(|(name, _)| (name.to_string(), asc))
            })
            .collect()
    }

    /// Effective lazy frame: base with all sort keys applied in priority order.
    fn effective_lf(&self) -> Option<LazyFrame> {
        let Source::Lazy(base) = &self.source else {
            return None;
        };
        let keys = self.sort_keys();
        if keys.is_empty() {
            return Some(base.clone());
        }
        let (names, descending): (Vec<String>, Vec<bool>) =
            keys.into_iter().map(|(name, asc)| (name, !asc)).unzip();
        Some(base.clone().sort(
            names,
            SortMultipleOptions::default().with_order_descending_multi(descending),
        ))
    }

    fn fetch(&self, offset: usize, height: usize) -> Result<DataFrame> {
        match &self.source {
            Source::Lazy(_) => {
                let lf = self.effective_lf().expect("lazy source");
                Self::fetch_lazy(&lf, offset, height)
            }
            Source::Lake(query) => lake_db::page_with(
                &query.conn,
                &query.source,
                &self.sort_keys(),
                offset,
                height,
            ),
        }
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
        let vp = self.viewport_rows;
        let (tx, rx) = mpsc::channel();

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
                let Ok(conn) = query.conn.try_clone() else { return rx };
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
        self.sort.clear();
        self.row_offset = 0;
        self.current_view = self.fetch(0, self.viewport_rows)?;
        Ok(())
    }

    pub fn scroll_to_offset(&mut self, offset: usize) -> Result<()> {
        let max = self.total_rows.saturating_sub(self.viewport_rows);
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
        let Ok(conn) = query.conn.try_clone() else { return };
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

                let Ok(mut stmt) = conn.prepare(&sql) else { break };
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

    fn search_lazy(
        &self,
        pattern: String,
        col_name: Option<String>,
        tx: mpsc::Sender<Vec<usize>>,
    ) {
        let Some(lf) = self.effective_lf() else { return };
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
