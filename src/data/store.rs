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

    pub fn scroll_down(&mut self, n: usize) -> Result<()> {
        let max = self.total_rows.saturating_sub(self.viewport_rows);
        self.row_offset = (self.row_offset + n).min(max);
        self.current_view = Self::fetch(&self.lf, self.row_offset, self.viewport_rows)?;
        Ok(())
    }

    pub fn scroll_up(&mut self, n: usize) -> Result<()> {
        self.row_offset = self.row_offset.saturating_sub(n);
        self.current_view = Self::fetch(&self.lf, self.row_offset, self.viewport_rows)?;
        Ok(())
    }

    pub fn scroll_to_top(&mut self) -> Result<()> {
        self.row_offset = 0;
        self.current_view = Self::fetch(&self.lf, self.row_offset, self.viewport_rows)?;
        Ok(())
    }

    pub fn scroll_to_bottom(&mut self) -> Result<()> {
        self.row_offset = self.total_rows.saturating_sub(self.viewport_rows);
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

    fn count_rows(lf: &LazyFrame) -> Result<usize> {
        let df = lf.clone().select([len().alias("n")]).collect()?;
        Ok(df.column("n")?.u32()?.get(0).unwrap_or(0) as usize)
    }
}
