use anyhow::Result;
use polars::prelude::*;
use std::path::Path;

pub enum FileFormat {
    Csv,
    Parquet,
    Unknown,
}

pub fn detect_format(path: &Path) -> FileFormat {
    match path.extension().and_then(|e| e.to_str()) {
        Some("csv") => FileFormat::Csv,
        Some("parquet") => FileFormat::Parquet,
        _ => FileFormat::Unknown,
    }
}

pub fn load(path: &Path) -> Result<LazyFrame> {
    let pl_path = PlRefPath::try_from_path(path)?;
    match detect_format(path) {
        FileFormat::Csv => Ok(LazyCsvReader::new(pl_path).finish()?),
        FileFormat::Parquet => Ok(LazyFrame::scan_parquet(pl_path, Default::default())?),
        FileFormat::Unknown => anyhow::bail!("unsupported file format (use .csv or .parquet)"),
    }
}
