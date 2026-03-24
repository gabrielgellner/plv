use std::sync::mpsc;
use std::thread::{self, yield_now};

use anyhow::Result;
use polars::prelude::*;

pub struct Store {
    lf: LazyFrame,
    pub schema: SchemaRef,
    pub total_rows: usize,
    pub row_offset: usize,
    pub viewport_rows: usize,
    pub current_view: DataFrame,
}

impl Store {
    pub fn new(mut lf: LazyFrame, viewport_rows: usize) -> Result<Self> {
        let schema = lf.collect_schema()?;
        let total_rows = Self::count_rows(&lf)?;
        let current_view = Self::fetch(&lf, 0, viewport_rows)?;
        Ok(Self {
            lf,
            schema,
            total_rows,
            row_offset: 0,
            viewport_rows,
            current_view,
        })
    }

    pub fn scroll_to_offset(&mut self, offset: usize) -> Result<()> {
        let max = self.total_rows.saturating_sub(self.viewport_rows);
        self.row_offset = offset.min(max);
        self.current_view = Self::fetch(&self.lf, self.row_offset, self.viewport_rows)?;
        Ok(())
    }

    pub fn resize(&mut self, new_height: usize) -> Result<()> {
        if self.viewport_rows != new_height && new_height > 0 {
            self.viewport_rows = new_height;
            self.current_view = Self::fetch(&self.lf, self.row_offset, self.viewport_rows)?;
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
        let lf = self.lf.clone();
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
                    .with_row_index("__idx__", None)
                    .slice(offset as i64, size as u32)
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
